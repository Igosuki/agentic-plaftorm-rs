//! Transport-neutral request pipeline shared by every front end (HTTP gateway, A2A).

use std::time::Instant;

use agentic_core::auth::{Claims, ROLE_ADMIN};
use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;
use serde_json::{Value, json};

use crate::{
    config::is_valid_use_case,
    coordinator::{RouteMethod, Routing},
    engine::{Attribution, TokenUsage},
    gateway::AppState,
    guard::{AttemptOutcome, GuardRequest},
    mcp::{Evidence, ToolSession},
    telemetry::{self, RequestOutcome, RequestRecord, UsageRecord},
};

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
    request_id: String,
    trace: Option<Box<Value>>,
}

impl ApiError {
    pub fn new(
        status: StatusCode,
        code: &'static str,
        message: impl Into<String>,
        request_id: &str,
    ) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            request_id: request_id.to_owned(),
            trace: None,
        }
    }

    pub fn with_trace(mut self, trace: Value) -> Self {
        self.trace = Some(Box::new(trace));
        self
    }

    pub fn status(&self) -> StatusCode {
        self.status
    }

    pub fn code(&self) -> &'static str {
        self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    pub fn trace(&self) -> Option<&Value> {
        self.trace.as_deref()
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut body = json!({
            "error": {"code": self.code, "message": self.message},
            "request_id": self.request_id,
        });
        if let Some(t) = self.trace {
            body["trace"] = *t;
        }
        (self.status, Json(body)).into_response()
    }
}

#[derive(Debug, Clone)]
pub struct RequestContext {
    pub request_id: String,
    pub user: String,
    pub team: String,
    pub roles: Vec<String>,
    /// Effective application: the `x-application` header, else the platform `app`.
    pub application: String,
    /// The `x-application` header alone; the only part that needs allowlist validation.
    pub application_override: Option<String>,
    pub use_case: Option<String>,
    pub claims: Claims,
}

impl RequestContext {
    pub(crate) fn require_admin(&self) -> Result<(), ApiError> {
        if self.claims.has_role(ROLE_ADMIN) {
            Ok(())
        } else {
            Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "forbidden",
                "the `admin` role is required",
                &self.request_id,
            ))
        }
    }
}

/// Verifies an `Authorization` header value (`Bearer <jwt>`) against `expected_aud` and the configured issuer.
/// The context has no application or use-case overrides and a fresh `request_id`; errors are 401 `unauthorized`.
pub fn authenticate(
    state: &AppState,
    authorization: Option<&str>,
    expected_aud: &str,
) -> Result<RequestContext, ApiError> {
    let request_id = uuid::Uuid::new_v4().to_string();
    let unauthorized =
        |m: &str| ApiError::new(StatusCode::UNAUTHORIZED, "unauthorized", m, &request_id);
    let token = authorization
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(|| unauthorized("missing bearer token"))?;
    let claims = state
        .keys
        .verify(token, expected_aud, &state.config.auth.issuer)
        .map_err(|_| unauthorized("invalid or expired token"))?;
    Ok(RequestContext {
        user: claims.sub.clone(),
        team: claims.team.clone(),
        roles: claims.roles.clone(),
        application: state.config.app.clone(),
        application_override: None,
        use_case: None,
        claims,
        request_id,
    })
}

/// `authenticate` against each audience in turn; the first that verifies wins, else the last error.
pub fn authenticate_any<S: AsRef<str>>(
    state: &AppState,
    authorization: Option<&str>,
    audiences: &[S],
) -> Result<RequestContext, ApiError> {
    let mut last = None;
    for aud in audiences {
        match authenticate(state, authorization, aud.as_ref()) {
            Ok(ctx) => return Ok(ctx),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| {
        ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "no accepted token audience is configured",
            &uuid::Uuid::new_v4().to_string(),
        )
    }))
}

#[derive(Debug, Clone)]
pub struct RunRequest {
    pub input: String,
    /// Validated against the application allowlist; `None` means the platform `app`.
    pub application: Option<String>,
    /// Validated against the use-case allowlist; `None` defaults to the routed agent's name.
    pub use_case: Option<String>,
    /// Names a registered specialist and skips the intent gate and coordinator.
    pub agent_hint: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PromptRef {
    pub id: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunResponse {
    pub agent: String,
    pub output: Value,
    pub prompt: PromptRef,
    pub request_id: String,
    pub trace: Value,
}

#[derive(Serialize)]
struct Usage {
    input_tokens: u64,
    output_tokens: u64,
    cost_usd: f64,
}

/// Rejected label values are echoed back, so cap and strip control characters first.
fn printable(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).take(64).collect()
}

fn classification_json(method: RouteMethod, scores: &crate::intent::Scores) -> Value {
    json!({"method": method.as_str(), "scores": scores})
}

fn count_attempts(attempts: &[crate::guard::AttemptRecord], outcome: AttemptOutcome) -> u64 {
    attempts.iter().filter(|a| a.outcome == outcome).count() as u64
}

#[tracing::instrument(name = "run_agent", skip_all, fields(request_id = %ctx.request_id, user = %ctx.user, team = %ctx.team))]
pub async fn run_pipeline(
    st: &AppState,
    ctx: &RequestContext,
    req: RunRequest,
) -> Result<RunResponse, ApiError> {
    let started = Instant::now();
    let bad_request = |m: String| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            m,
            &ctx.request_id,
        )
    };
    if req.input.trim().is_empty() {
        return Err(bad_request("`input` must not be empty".into()));
    }
    let app = match req.application.as_deref() {
        Some(a) if st.applications.contains(a) => a.to_owned(),
        Some(a) => {
            return Err(bad_request(format!(
                "`application` `{}` is not allowed",
                printable(a)
            )));
        }
        None => st.config.app.clone(),
    };
    let explicit_use_case = req.use_case.as_deref();
    if let Some(u) = explicit_use_case
        && !(is_valid_use_case(u) && st.use_cases.contains(u))
    {
        return Err(bad_request(format!(
            "`use_case` `{}` is not allowed",
            printable(u)
        )));
    }
    if let Some(h) = req.agent_hint.as_deref()
        && !st.specialists.contains_key(h)
    {
        return Err(bad_request(format!(
            "`agent_hint` `{}` is not a registered agent",
            printable(h)
        )));
    }
    let latency = || u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let mut record = RequestRecord {
        app: app.clone(),
        user: ctx.user.clone(),
        team: ctx.team.clone(),
        use_case: explicit_use_case.unwrap_or("none").to_owned(),
        agent: "none".into(),
        prompt_version: "none".into(),
        model: String::new(),
        outcome: RequestOutcome::Ok,
        input_tokens: 0,
        output_tokens: 0,
        cost_usd: 0.0,
        latency_ms: 0,
        hallucinations: 0,
        schema_violations: 0,
    };
    let finish = |mut r: RequestRecord, outcome: RequestOutcome| {
        r.outcome = outcome;
        r.latency_ms = latency();
        telemetry::record_request(&r);
        st.store.record(&r);
    };

    if !st.limiter.try_acquire(&ctx.team) {
        finish(record, RequestOutcome::RateLimited);
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "team request quota exceeded",
            &ctx.request_id,
        ));
    }

    let routing = if let Some(agent) = req.agent_hint {
        Routing {
            agent,
            method: RouteMethod::Explicit,
            scores: Default::default(),
            usage: TokenUsage::default(),
        }
    } else {
        let routed = st
            .coordinator
            .route(
                &req.input,
                Attribution {
                    user: ctx.user.clone(),
                    team: ctx.team.clone(),
                    application: app.clone(),
                    use_case: record.use_case.clone(),
                    agent: String::new(),
                    prompt_version: String::new(),
                    request_id: ctx.request_id.clone(),
                },
            )
            .await;
        let coordinator_usage = match &routed {
            Ok(r) => r.usage,
            Err(u) => u.usage,
        };
        if coordinator_usage.total() > 0 {
            let model = st.config.models.coordinator();
            let cost = st.pricing.cost_usd(
                model,
                coordinator_usage.input_tokens,
                coordinator_usage.output_tokens,
            );
            let usage = UsageRecord {
                app: &app,
                team: &ctx.team,
                use_case: &record.use_case,
                agent: "coordinator",
                model,
                input_tokens: coordinator_usage.input_tokens,
                output_tokens: coordinator_usage.output_tokens,
                cost_usd: cost,
            };
            telemetry::record_usage(&usage);
            st.store.add_usage(&ctx.user, &usage);
        }
        match routed {
            Ok(r) => r,
            Err(u) => {
                finish(record, RequestOutcome::Unclassified);
                return Err(ApiError::new(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "unclassified",
                    "the request does not match any available agent",
                    &ctx.request_id,
                )
                .with_trace(
                    json!({"classification": classification_json(RouteMethod::Keywords, &u.scores)}),
                ));
            }
        }
    };

    let internal = |m: String| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            m,
            &ctx.request_id,
        )
    };
    let resolved = (|| {
        let def = st
            .specialists
            .get(&routing.agent)
            .ok_or_else(|| internal(format!("agent `{}` is not registered", routing.agent)))?;
        let prompt = st
            .prompts
            .resolve_live(&def.prompt_id)
            .ok_or_else(|| internal(format!("prompt `{}` is not available", def.prompt_id)))?;
        let model_spec = prompt
            .model
            .clone()
            .unwrap_or_else(|| st.config.models.default.clone());
        let engine_name = def.engine.as_deref().unwrap_or(&st.config.engine);
        let engine = st
            .engines
            .get(engine_name)
            .ok_or_else(|| internal(format!("engine `{engine_name}` is not registered")))?;
        engine
            .check_model(&model_spec)
            .map_err(|e| internal(e.to_string()))?;
        Ok((def, prompt, model_spec, engine.as_ref()))
    })();
    let (def, prompt, model_spec, engine) = match resolved {
        Ok(r) => r,
        Err(e) => {
            finish(record, RequestOutcome::InternalError);
            return Err(e);
        }
    };

    record.agent = def.name.clone();
    record.prompt_version = prompt.version.clone();
    record.model = model_spec.clone();
    record.use_case = explicit_use_case.unwrap_or(&def.name).to_owned();

    let evidence = Evidence::new();
    let session = ToolSession {
        claims: ctx.claims.clone(),
        request_id: ctx.request_id.clone(),
        app: app.clone(),
        agent: def.name.clone(),
        evidence: evidence.clone(),
    };
    let result = def
        .runner
        .run(GuardRequest {
            engine,
            model_spec: &model_spec,
            preamble: &prompt.template,
            temperature: prompt.temperature,
            max_turns: def.max_turns,
            tools: st.hub.tools_for(&def.mcp_servers, &session),
            input: &req.input,
            evidence: &evidence,
            config: &st.config.guard,
            app: &app,
            agent: &def.name,
            use_case: &record.use_case,
            prompt_version: &prompt.version,
            caller: Some(ctx.claims.clone()),
            request_id: &ctx.request_id,
        })
        .await;

    let cost = st.pricing.cost_usd(
        &model_spec,
        result.usage.input_tokens,
        result.usage.output_tokens,
    );
    record.input_tokens = result.usage.input_tokens;
    record.output_tokens = result.usage.output_tokens;
    record.cost_usd = cost;
    record.hallucinations = count_attempts(&result.attempts, AttemptOutcome::Hallucination);
    record.schema_violations = count_attempts(&result.attempts, AttemptOutcome::SchemaViolation);

    let tool_calls: Vec<Value> = evidence
        .calls()
        .iter()
        .map(|c| json!({"server": c.server, "tool": c.tool, "outcome": c.outcome, "latency_ms": c.latency_ms}))
        .collect();
    let trace = json!({
        "classification": classification_json(routing.method, &routing.scores),
        "engine": engine.name(),
        "attempts": result.attempts,
        "tool_calls": tool_calls,
        "usage": Usage { input_tokens: result.usage.input_tokens, output_tokens: result.usage.output_tokens, cost_usd: cost },
        "latency_ms": latency(),
    });

    match result.output {
        Some(output) => {
            finish(record, RequestOutcome::Ok);
            Ok(RunResponse {
                request_id: ctx.request_id.clone(),
                agent: def.name.clone(),
                prompt: PromptRef {
                    id: prompt.id.clone(),
                    version: prompt.version.clone(),
                },
                output,
                trace,
            })
        }
        None => {
            finish(record, RequestOutcome::AgentFailed);
            let last = result
                .attempts
                .last()
                .and_then(|a| a.detail.clone())
                .unwrap_or_else(|| "no attempt succeeded".into());
            Err(ApiError::new(
                StatusCode::BAD_GATEWAY,
                "agent_failed",
                format!("agent could not produce a valid answer: {last}"),
                &ctx.request_id,
            )
            .with_trace(trace))
        }
    }
}
