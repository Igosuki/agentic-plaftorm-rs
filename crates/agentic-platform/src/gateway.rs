use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    convert::Infallible,
    net::SocketAddr,
    sync::Arc,
};

use agentic_core::auth::{AUD_GATEWAY, JwtKeys};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{ConnectInfo, FromRequestParts, Path, State},
    http::{HeaderMap, StatusCode, header, request::Parts},
    response::IntoResponse,
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use tower_http::trace::TraceLayer;

pub use crate::pipeline::{ApiError, RequestContext};
use crate::{
    config::PlatformConfig,
    coordinator::Coordinator,
    engine::AgentEngine,
    llm::Pricing,
    mcp::McpHub,
    pipeline::{RunRequest, RunResponse, authenticate, run_pipeline},
    prompts::{PromptError, PromptRegistry},
    specialist::SpecialistDef,
    telemetry::MetricsStore,
};

pub mod rate_limit {
    use std::{collections::HashMap, sync::Mutex, time::Instant};

    const MAX_BUCKETS: usize = 10_000;

    struct Bucket {
        tokens: f64,
        last: Instant,
    }

    /// Per-team token bucket: burst equals the per-minute quota, refilled continuously. A quota of 0 disables limiting.
    pub struct RateLimiter {
        per_minute: u32,
        buckets: Mutex<HashMap<String, Bucket>>,
    }

    impl RateLimiter {
        pub fn new(per_minute: u32) -> Self {
            Self {
                per_minute,
                buckets: Mutex::default(),
            }
        }

        pub fn try_acquire(&self, team: &str) -> bool {
            self.try_acquire_at(team, Instant::now())
        }

        pub fn try_acquire_at(&self, team: &str, now: Instant) -> bool {
            if self.per_minute == 0 {
                return true;
            }
            let cap = f64::from(self.per_minute);
            let mut buckets = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
            if buckets.len() >= MAX_BUCKETS {
                // A bucket idle long enough to have refilled is indistinguishable from a fresh one.
                buckets.retain(|_, b| {
                    b.tokens + now.saturating_duration_since(b.last).as_secs_f64() * cap / 60.0
                        < cap
                });
            }
            let b = buckets.entry(team.to_owned()).or_insert(Bucket {
                tokens: cap,
                last: now,
            });
            let elapsed = now.saturating_duration_since(b.last).as_secs_f64();
            b.tokens = (b.tokens + elapsed * cap / 60.0).min(cap);
            b.last = now;
            if b.tokens >= 1.0 {
                b.tokens -= 1.0;
                true
            } else {
                false
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use std::time::Duration;

        use super::*;

        #[test]
        fn bursts_then_refills_per_team() {
            let rl = RateLimiter::new(2);
            let t0 = Instant::now();
            assert!(rl.try_acquire_at("a", t0));
            assert!(rl.try_acquire_at("a", t0));
            assert!(!rl.try_acquire_at("a", t0));
            assert!(rl.try_acquire_at("b", t0));
            assert!(!rl.try_acquire_at("a", t0 + Duration::from_secs(10)));
            assert!(rl.try_acquire_at("a", t0 + Duration::from_secs(31)));
        }

        #[test]
        fn idle_buckets_are_pruned() {
            let rl = RateLimiter::new(1);
            let t0 = Instant::now();
            for i in 0..MAX_BUCKETS {
                assert!(rl.try_acquire_at(&format!("k{i}"), t0));
            }
            assert!(rl.try_acquire_at("late", t0 + Duration::from_secs(120)));
            assert_eq!(rl.buckets.lock().unwrap().len(), 1);
        }

        #[test]
        fn zero_disables() {
            let rl = RateLimiter::new(0);
            assert!((0..1000).all(|_| rl.try_acquire("a")));
        }
    }
}

pub struct AppState {
    pub config: PlatformConfig,
    pub keys: JwtKeys,
    pub users_by_key: HashMap<String, crate::config::User>,
    pub prompts: PromptRegistry,
    pub coordinator: Coordinator,
    pub specialists: BTreeMap<String, SpecialistDef>,
    pub hub: McpHub,
    pub policy: Arc<agentic_core::policy::Policy>,
    pub engines: HashMap<String, Arc<dyn AgentEngine>>,
    pub pricing: Pricing,
    pub limiter: rate_limit::RateLimiter,
    pub auth_limiter: rate_limit::RateLimiter,
    pub applications: BTreeSet<String>,
    pub use_cases: BTreeSet<String>,
    pub store: MetricsStore,
    pub prometheus: metrics_exporter_prometheus::PrometheusHandle,
}

pub type SharedState = Arc<AppState>;

fn header_str<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
    h.get(name)
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty())
}

impl FromRequestParts<SharedState> for RequestContext {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &SharedState) -> Result<Self, ApiError> {
        let mut ctx = authenticate(
            state,
            header_str(&parts.headers, header::AUTHORIZATION.as_str()),
            AUD_GATEWAY,
        )?;
        ctx.application_override = header_str(&parts.headers, "x-application").map(str::to_owned);
        if let Some(a) = &ctx.application_override {
            ctx.application.clone_from(a);
        }
        ctx.use_case = header_str(&parts.headers, "x-use-case").map(str::to_owned);
        Ok(ctx)
    }
}

/// The main API; includes `/metrics` unless `metrics_listen` moves it to its own listener.
pub fn router(state: SharedState) -> Router {
    let main = api_routes();
    let main = if state.config.metrics_listen().is_none() {
        main.route("/metrics", get(prometheus_text))
    } else {
        main
    };
    main.layer(TraceLayer::new_for_http()).with_state(state)
}

pub fn metrics_router(state: SharedState) -> Router {
    Router::new()
        .route("/metrics", get(prometheus_text))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Client IP for throttling; the peer address only, never forwarded headers a caller could spoof.
struct ClientKey(String);

impl<S: Send + Sync> FromRequestParts<S> for ClientKey {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Infallible> {
        Ok(Self(
            parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map_or_else(|| "unknown".to_owned(), |c| c.0.ip().to_string()),
        ))
    }
}

fn api_routes() -> Router<SharedState> {
    Router::new()
        .route("/healthz", get(|| async { Json(json!({"status": "ok"})) }))
        .route("/auth/token", post(auth_token))
        .route("/v1/agents", get(list_agents))
        .route("/v1/agents/run", post(run_agent))
        .route("/v1/metrics/me", get(metrics_me))
        .route("/v1/metrics/teams", get(metrics_teams))
        .route("/admin/prompts", get(admin_prompts))
        .route("/admin/prompts/{id}", get(admin_prompt))
        .route("/admin/prompts/{id}/live", post(admin_set_live))
        .route("/admin/prompts/{id}/rollback", post(admin_rollback))
}

fn parse_body<T: serde::de::DeserializeOwned>(
    body: &Bytes,
    request_id: &str,
) -> Result<T, ApiError> {
    serde_json::from_slice(body).map_err(|e| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            e.to_string(),
            request_id,
        )
    })
}

async fn prometheus_text(State(st): State<SharedState>) -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        st.prometheus.render(),
    )
}

#[derive(Deserialize)]
struct TokenRequest {
    api_key: String,
}

async fn auth_token(
    State(st): State<SharedState>,
    ClientKey(client): ClientKey,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let request_id = uuid::Uuid::new_v4().to_string();
    if !st.auth_limiter.try_acquire(&client) {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "too many token requests from this address",
            &request_id,
        ));
    }
    let req: TokenRequest = parse_body(&body, &request_id)?;
    let user = st.users_by_key.get(&req.api_key).ok_or_else(|| {
        ApiError::new(
            StatusCode::UNAUTHORIZED,
            "invalid_api_key",
            "unknown api key",
            &request_id,
        )
    })?;
    let ttl = st.config.auth.token_ttl_secs;
    let token = st
        .keys
        .issue_user_token(
            &user.id,
            &user.team,
            &user.roles,
            &st.config.auth.issuer,
            ttl,
        )
        .map_err(|e| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                e.to_string(),
                &request_id,
            )
        })?;
    Ok(Json(
        json!({"access_token": token, "token_type": "Bearer", "expires_in": ttl}),
    ))
}

async fn list_agents(State(st): State<SharedState>, _ctx: RequestContext) -> Json<Value> {
    Json(Value::Array(
        st.specialists
            .values()
            .map(|s| {
                json!({
                    "name": s.name,
                    "description": s.description,
                    "prompt_id": s.prompt_id,
                    "mcp_servers": s.mcp_servers,
                })
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
struct RunBody {
    input: String,
    application: Option<String>,
    use_case: Option<String>,
}

async fn run_agent(
    State(st): State<SharedState>,
    ctx: RequestContext,
    body: Bytes,
) -> Result<Json<RunResponse>, ApiError> {
    let body: RunBody = parse_body(&body, &ctx.request_id)?;
    let req = RunRequest {
        input: body.input,
        application: body
            .application
            .or_else(|| ctx.application_override.clone()),
        use_case: body.use_case.or_else(|| ctx.use_case.clone()),
        agent_hint: None,
    };
    run_pipeline(&st, &ctx, req).await.map(Json)
}

async fn metrics_me(
    State(st): State<SharedState>,
    ctx: RequestContext,
) -> Json<crate::telemetry::UserMetrics> {
    Json(st.store.user(&ctx.user, &ctx.team))
}

async fn metrics_teams(
    State(st): State<SharedState>,
    ctx: RequestContext,
) -> Result<Json<Vec<crate::telemetry::TeamMetrics>>, ApiError> {
    ctx.require_admin()?;
    Ok(Json(st.store.teams()))
}

async fn admin_prompts(
    State(st): State<SharedState>,
    ctx: RequestContext,
) -> Result<Json<Value>, ApiError> {
    ctx.require_admin()?;
    Ok(Json(json!(st.prompts.list())))
}

fn prompt_error(e: PromptError, request_id: &str) -> ApiError {
    let (status, code) = match &e {
        PromptError::UnknownPrompt(_) => (StatusCode::NOT_FOUND, "prompt_not_found"),
        PromptError::UnknownVersion { .. } => (StatusCode::NOT_FOUND, "version_not_found"),
        PromptError::NoHistory(_) => (StatusCode::CONFLICT, "no_history"),
        PromptError::Persist(_) => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
    };
    ApiError::new(status, code, e.to_string(), request_id)
}

async fn admin_prompt(
    State(st): State<SharedState>,
    ctx: RequestContext,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    ctx.require_admin()?;
    st.prompts
        .get(&id)
        .map(|d| Json(json!(d)))
        .ok_or_else(|| prompt_error(PromptError::UnknownPrompt(id), &ctx.request_id))
}

#[derive(Deserialize)]
struct LiveBody {
    version: String,
    /// `false` leaves the rollback history untouched (used to restore a temporary override).
    #[serde(default = "default_true")]
    record_history: bool,
}

fn default_true() -> bool {
    true
}

async fn admin_set_live(
    State(st): State<SharedState>,
    ctx: RequestContext,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    ctx.require_admin()?;
    let body: LiveBody = parse_body(&body, &ctx.request_id)?;
    let moved = if body.record_history {
        st.prompts.set_live(&id, &body.version).await
    } else {
        st.prompts.set_live_untracked(&id, &body.version).await
    };
    moved.map_err(|e| prompt_error(e, &ctx.request_id))?;
    tracing::info!(prompt = %id, version = %body.version, by = %ctx.user, "prompt live pointer moved");
    Ok(Json(json!({"id": id, "live": body.version})))
}

async fn admin_rollback(
    State(st): State<SharedState>,
    ctx: RequestContext,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    ctx.require_admin()?;
    let live = st
        .prompts
        .rollback(&id)
        .await
        .map_err(|e| prompt_error(e, &ctx.request_id))?;
    tracing::info!(prompt = %id, version = %live, by = %ctx.user, "prompt rolled back");
    Ok(Json(json!({"id": id, "live": live})))
}
