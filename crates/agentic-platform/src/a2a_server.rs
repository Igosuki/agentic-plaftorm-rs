//! Serves the platform as an A2A v1.0 agent: card and JSON-RPC binding over `pipeline::run_pipeline`.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
    time::Duration,
};

use a2a_rs::{
    adapter::{
        InMemoryTaskStorage, JsonRpcAdapter, SimpleAgentInfo,
        business::{Responder, ResponderMessageHandler},
        jsonrpc_router,
        streaming::InMemoryStreamingHandler,
        transport::jsonrpc::{JsonRpcId, JsonRpcResponse, a2a_to_jsonrpc, methods},
    },
    domain::{A2AError, Message, Part, RetentionPolicy, Role, Task, TaskId, TaskState},
    port::{AsyncRetention, AsyncTaskLifecycle, AuthPrincipal, NoopPushNotifier},
    services::server::AgentInfoProvider,
};
use async_trait::async_trait;
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::State,
    http::{Request, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use buffa_types::google::protobuf::Value as PbValue;
use chrono::{DateTime, Utc};
use serde_json::{Value, json};

use crate::{
    gateway::SharedState,
    pipeline::{ApiError, RequestContext, RunRequest, RunResponse, authenticate_any, run_pipeline},
};

pub const AUTO_SKILL: &str = "auto";

const MAX_RPC_BODY: usize = 4 << 20;
const MAX_SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// The only methods served; list, push-notification-config and extended-card methods are not.
const ALLOWED_METHODS: [&str; 5] = [
    methods::SEND_MESSAGE,
    methods::SEND_STREAMING_MESSAGE,
    methods::GET_TASK,
    methods::CANCEL_TASK,
    methods::SUBSCRIBE_TO_TASK,
];

tokio::task_local! {
    static CALLER: RequestContext;
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Owner {
    sub: String,
    team: String,
}

impl Owner {
    fn of(ctx: &RequestContext) -> Self {
        Self {
            sub: ctx.claims.sub.clone(),
            team: ctx.claims.team.clone(),
        }
    }
}

struct Entry {
    owner: Owner,
    touched: DateTime<Utc>,
}

#[derive(Default)]
struct Maps {
    tasks: HashMap<String, Entry>,
    contexts: HashMap<String, Entry>,
}

/// Task and context id to the caller that created it. The a2a-rs store has no principal filter.
#[derive(Default)]
struct Ownership(Mutex<Maps>);

fn claim(map: &mut HashMap<String, Entry>, id: &str, who: &Owner, now: DateTime<Utc>) -> bool {
    let e = map.entry(id.to_owned()).or_insert_with(|| Entry {
        owner: who.clone(),
        touched: now,
    });
    let mine = e.owner == *who;
    if mine {
        e.touched = now;
    }
    mine
}

impl Ownership {
    fn maps(&self) -> std::sync::MutexGuard<'_, Maps> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Records `who` as owner of an unowned id; `false` when someone else owns it.
    fn claim_task(&self, id: &str, who: &Owner, now: DateTime<Utc>) -> bool {
        claim(&mut self.maps().tasks, id, who, now)
    }

    fn claim_context(&self, id: &str, who: &Owner, now: DateTime<Utc>) -> bool {
        claim(&mut self.maps().contexts, id, who, now)
    }

    fn owns_task(&self, id: &str, who: &Owner, now: DateTime<Utc>) -> bool {
        match self.maps().tasks.get_mut(id) {
            Some(e) if e.owner == *who => {
                e.touched = now;
                true
            }
            _ => false,
        }
    }

    fn sweep(&self, cutoff: DateTime<Utc>) -> usize {
        let mut m = self.maps();
        let before = m.tasks.len() + m.contexts.len();
        m.tasks.retain(|_, e| e.touched >= cutoff);
        m.contexts.retain(|_, e| e.touched >= cutoff);
        before - m.tasks.len() - m.contexts.len()
    }
}

fn not_found(id: &str) -> A2AError {
    A2AError::TaskNotFound(id.to_owned())
}

fn str_field<'a>(obj: &'a Value, camel: &str, snake: &str) -> Vec<&'a str> {
    [camel, snake]
        .into_iter()
        .filter_map(|k| obj.get(k).and_then(Value::as_str))
        .filter(|s| !s.is_empty())
        .collect()
}

/// Applies the method allowlist and ownership rules to one decoded JSON-RPC call.
/// Both spellings of each id field are checked, since ProtoJSON parsers accept either.
fn authorize(
    owners: &Ownership,
    who: &Owner,
    call: &Value,
    now: DateTime<Utc>,
) -> Result<(), A2AError> {
    let Some(method) = call.get("method").and_then(Value::as_str) else {
        return Ok(());
    };
    if !ALLOWED_METHODS.contains(&method) {
        return Err(A2AError::MethodNotFound(method.to_owned()));
    }
    let params = call.get("params").unwrap_or(&Value::Null);
    match method {
        methods::GET_TASK | methods::CANCEL_TASK | methods::SUBSCRIBE_TO_TASK => {
            match params.get("id").and_then(Value::as_str) {
                Some(id) if !owners.owns_task(id, who, now) => Err(not_found(id)),
                _ => Ok(()),
            }
        }
        _ => {
            if params
                .pointer("/configuration/taskPushNotificationConfig")
                .or_else(|| params.pointer("/configuration/task_push_notification_config"))
                .is_some_and(|c| !c.is_null())
            {
                return Err(A2AError::PushNotificationNotSupported);
            }
            let message = params.get("message").unwrap_or(&Value::Null);
            for id in str_field(message, "taskId", "task_id") {
                if !owners.claim_task(id, who, now) {
                    return Err(not_found(id));
                }
            }
            for id in str_field(message, "contextId", "context_id") {
                if !owners.claim_context(id, who, now) {
                    return Err(not_found(id));
                }
            }
            Ok(())
        }
    }
}

fn rpc_error(call: &Value, e: &A2AError) -> Response {
    let id = call
        .get("id")
        .and_then(|v| serde_json::from_value::<JsonRpcId>(v.clone()).ok())
        .unwrap_or_default();
    Json(JsonRpcResponse::err(id, a2a_to_jsonrpc(e))).into_response()
}

async fn guard_rpc(
    State(owners): State<Arc<Ownership>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let Some(who) = req.extensions().get::<RequestContext>().map(Owner::of) else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let (parts, body) = req.into_parts();
    let Ok(bytes) = to_bytes(body, MAX_RPC_BODY).await else {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    };
    let Ok(call) = serde_json::from_slice::<Value>(&bytes) else {
        return next
            .run(Request::from_parts(parts, Body::from(bytes)))
            .await;
    };
    if let Err(e) = authorize(&owners, &who, &call, Utc::now()) {
        return rpc_error(&call, &e);
    }
    // Forward the checked document, so a duplicate-key parse difference cannot skip the check.
    let checked = serde_json::to_vec(&call).map_or(bytes, Into::into);
    let mut parts = parts;
    parts.headers.remove(header::CONTENT_LENGTH);
    next.run(Request::from_parts(parts, Body::from(checked)))
        .await
}

struct PlatformResponder {
    state: SharedState,
    storage: InMemoryTaskStorage,
    owners: Arc<Ownership>,
}

/// Marks the task failed if the handler future is dropped mid-run, so a disconnected client does not leave a
/// `working` task that retention never reclaims.
struct FailOnDrop {
    storage: InMemoryTaskStorage,
    task: Option<TaskId>,
}

impl FailOnDrop {
    fn disarm(&mut self) {
        self.task = None;
    }
}

impl Drop for FailOnDrop {
    fn drop(&mut self) {
        let (Some(id), Ok(rt)) = (self.task.take(), tokio::runtime::Handle::try_current()) else {
            return;
        };
        let storage = self.storage.clone();
        rt.spawn(async move {
            if let Err(e) = storage
                .update_status(&id, TaskState::TASK_STATE_FAILED, None)
                .await
            {
                tracing::warn!(task = %id, error = %e, "could not fail an abandoned a2a task");
            }
        });
    }
}

fn metadata_of(m: &Message) -> serde_json::Map<String, Value> {
    m.metadata
        .as_option()
        .and_then(|s| serde_json::to_value(s).ok())
        .and_then(|v| match v {
            Value::Object(o) => Some(o),
            _ => None,
        })
        .unwrap_or_default()
}

fn meta_str(meta: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    meta.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn input_text(m: &Message) -> String {
    m.parts
        .iter()
        .filter_map(Part::get_text)
        .collect::<Vec<_>>()
        .join("\n")
}

/// `skill`/`agent` metadata names a specialist; `auto` and absence both mean intent routing.
fn run_request(m: &Message) -> RunRequest {
    let meta = metadata_of(m);
    let hint = meta_str(&meta, "skill")
        .or_else(|| meta_str(&meta, "agent"))
        .filter(|h| h != AUTO_SKILL);
    RunRequest {
        input: input_text(m),
        application: meta_str(&meta, "application"),
        use_case: meta_str(&meta, "use_case"),
        agent_hint: hint,
        ui: None,
    }
}

fn reply(task: &Task, context_id: &str, text: String, data: Value) -> Result<Message, A2AError> {
    let data: PbValue =
        serde_json::from_value(data).map_err(|e| A2AError::Internal(e.to_string()))?;
    Ok(Message::builder()
        .role(Role::ROLE_AGENT)
        .parts(vec![Part::text(text), Part::data(data)])
        .message_id(uuid::Uuid::new_v4().to_string())
        .task_id(task.id.clone())
        .context_id(context_id.to_owned())
        .build())
}

/// Only what a caller needs from the pipeline's trace; attempt and tool-call detail stay in logs and metrics.
fn trimmed_trace(trace: &Value) -> Value {
    json!({"usage": trace["usage"], "latency_ms": trace["latency_ms"]})
}

fn success_data(run: &RunResponse) -> Value {
    json!({
        "output": run.output,
        "agent": run.agent,
        "prompt": run.prompt,
        "request_id": run.request_id,
        "trace": trimmed_trace(&run.trace),
    })
}

#[async_trait]
impl Responder for PlatformResponder {
    async fn respond(
        &self,
        message: &Message,
        task: &Task,
    ) -> Result<(Message, TaskState), A2AError> {
        let ctx = CALLER.try_with(RequestContext::clone).map_err(|_| {
            A2AError::Internal("request reached the handler unauthenticated".into())
        })?;
        let who = Owner::of(&ctx);
        let now = Utc::now();
        if !self.owners.claim_task(&task.id, &who, now)
            || !self.owners.claim_context(&task.context_id, &who, now)
        {
            return Err(not_found(&task.id));
        }
        let mut guard = FailOnDrop {
            storage: self.storage.clone(),
            task: task.id.parse().ok(),
        };
        let context_id = message.context_id.clone();
        let outcome = run_pipeline(&self.state, &ctx, run_request(message)).await;
        guard.disarm();
        match outcome {
            Ok(run) => {
                let text = serde_json::to_string(&run.output)
                    .map_err(|e| A2AError::Internal(e.to_string()))?;
                Ok((
                    reply(task, &context_id, text, success_data(&run))?,
                    TaskState::TASK_STATE_COMPLETED,
                ))
            }
            Err(e) => {
                let text = format!("{}: {}", e.code(), e.message());
                let data = json!({
                    "error": {"code": e.code(), "message": e.message()},
                    "request_id": e.request_id(),
                    "trace": e.trace().map(trimmed_trace),
                });
                let state = if e.status().is_client_error() {
                    TaskState::TASK_STATE_REJECTED
                } else {
                    TaskState::TASK_STATE_FAILED
                };
                Ok((reply(task, &context_id, text, data)?, state))
            }
        }
    }
}

fn public_url(state: &SharedState) -> String {
    let cfg = &state.config.a2a.server;
    cfg.public_url.clone().unwrap_or_else(|| {
        let listen = state.config.listen.replace("0.0.0.0", "localhost");
        format!("http://{listen}{}", cfg.path)
    })
}

fn agent_info(state: &SharedState) -> SimpleAgentInfo {
    let mut info = SimpleAgentInfo::new(state.config.app.clone(), public_url(state))
        .with_description(format!("{} agent platform", state.config.app))
        .with_streaming()
        .add_skill(
            AUTO_SKILL.into(),
            "Route by intent".into(),
            Some("Picks the specialist from the request text".into()),
            vec!["routing".into()],
        );
    for s in state.specialists.values() {
        let mut tags = state.coordinator.classifier().top_keywords(&s.name, 5);
        if tags.is_empty() {
            tags.push(s.name.clone());
        }
        let description = (!s.description.is_empty()).then(|| s.description.clone());
        info = info.add_skill(s.name.clone(), s.name.clone(), description, tags);
    }
    info
}

async fn card(State(info): State<Arc<SimpleAgentInfo>>) -> Response {
    match info.get_agent_card().await {
        Ok(mut c) => {
            c.capabilities.get_or_insert_default().push_notifications = Some(false);
            Json(c).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn require_auth(
    State(state): State<SharedState>,
    mut req: Request<Body>,
    next: Next,
) -> Response {
    let header_value = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    match authenticate_any(
        &state,
        header_value,
        &state.config.a2a.server.audiences(&state.config.app),
    ) {
        Ok(ctx) => {
            req.extensions_mut().insert(
                AuthPrincipal::new(ctx.claims.sub.clone(), "jwt".into())
                    .with_attribute("team".into(), ctx.claims.team.clone()),
            );
            req.extensions_mut().insert(ctx.clone());
            CALLER.scope(ctx, next.run(req)).await
        }
        Err(e) => ApiError::into_response(e),
    }
}

/// Drops tasks, contexts and ownership entries idle for `ttl`; running tasks are kept by the store.
async fn sweep_once(
    storage: &InMemoryTaskStorage,
    owners: &Ownership,
    ttl: Duration,
    now: DateTime<Utc>,
) {
    let policy = RetentionPolicy::keep_everything().delete_contexts_idle_for(ttl);
    match storage.sweep(&policy, now).await {
        Ok(swept) if !swept.is_empty() => {
            tracing::debug!(
                tasks = swept.tasks,
                contexts = swept.contexts,
                "swept idle a2a tasks"
            );
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "a2a retention sweep failed"),
    }
    if let Ok(window) = chrono::TimeDelta::from_std(ttl) {
        owners.sweep(now - window);
    }
}

fn sweep_interval(ttl: Duration) -> Duration {
    (ttl / 4)
        .min(MAX_SWEEP_INTERVAL)
        .max(Duration::from_secs(1))
}

/// Ends when the router, and with it the ownership map, is dropped.
fn spawn_sweeper(storage: InMemoryTaskStorage, owners: &Arc<Ownership>, ttl: Duration) {
    let Ok(rt) = tokio::runtime::Handle::try_current() else {
        tracing::warn!("no tokio runtime: a2a task retention sweeper not started");
        return;
    };
    let owners: Weak<Ownership> = Arc::downgrade(owners);
    rt.spawn(async move {
        let mut tick = tokio::time::interval(sweep_interval(ttl));
        tick.tick().await;
        loop {
            tick.tick().await;
            let Some(owners) = owners.upgrade() else {
                return;
            };
            sweep_once(&storage, &owners, ttl, Utc::now()).await;
        }
    });
}

/// Card routes (public) and the JSON-RPC binding under the configured path (bearer-protected).
pub fn router(state: &SharedState) -> Router {
    let storage = InMemoryTaskStorage::new();
    let streaming = InMemoryStreamingHandler::new();
    let owners = Arc::new(Ownership::default());
    let handler = ResponderMessageHandler::new(
        storage.clone(),
        streaming.clone(),
        NoopPushNotifier,
        PlatformResponder {
            state: state.clone(),
            storage: storage.clone(),
            owners: owners.clone(),
        },
    );
    let info = agent_info(state);
    let adapter = Arc::new(
        JsonRpcAdapter::new(handler, storage.clone(), storage.clone(), info.clone())
            .with_streaming_handler(streaming),
    );
    spawn_sweeper(
        storage,
        &owners,
        Duration::from_secs(state.config.a2a.server.task_ttl_secs),
    );
    let rpc = jsonrpc_router(adapter)
        .layer(middleware::from_fn_with_state(owners, guard_rpc))
        .layer(middleware::from_fn_with_state(state.clone(), require_auth));
    let card_router = Router::new()
        .route("/.well-known/agent-card.json", get(card))
        .with_state(Arc::new(info));
    let path = state.config.a2a.server.path.trim_end_matches('/');
    // The a2a-rs client resolves the card relative to the URL it was given, so serve it there too.
    Router::new()
        .merge(card_router.clone())
        .nest(path, rpc.merge(card_router))
}

#[cfg(test)]
mod tests {
    use a2a_rs::domain::{ContextId, TaskStateExt};

    use super::*;

    fn owner(sub: &str) -> Owner {
        Owner {
            sub: sub.into(),
            team: "t".into(),
        }
    }

    async fn task_in(storage: &InMemoryTaskStorage, id: &str, state: TaskState) -> TaskId {
        let tid: TaskId = id.parse().unwrap();
        storage.create(&tid, &ContextId::generate()).await.unwrap();
        storage.update_status(&tid, state, None).await.unwrap();
        tid
    }

    #[tokio::test]
    async fn sweep_drops_idle_settled_tasks_and_their_owners_but_not_running_ones() {
        let storage = InMemoryTaskStorage::new();
        let owners = Ownership::default();
        let done = task_in(&storage, "done", TaskState::TASK_STATE_COMPLETED).await;
        let running = task_in(&storage, "running", TaskState::TASK_STATE_WORKING).await;
        let now = Utc::now();
        for id in ["done", "running"] {
            assert!(owners.claim_task(id, &owner("alice"), now));
        }
        let ttl = Duration::from_secs(60);

        sweep_once(&storage, &owners, ttl, now + chrono::TimeDelta::seconds(30)).await;
        assert!(storage.exists(&done).await.unwrap());
        assert!(owners.owns_task("done", &owner("alice"), now));

        let later = now + chrono::TimeDelta::seconds(61);
        sweep_once(&storage, &owners, ttl, later).await;
        assert!(!storage.exists(&done).await.unwrap(), "idle task is swept");
        assert!(
            storage.exists(&running).await.unwrap(),
            "a running task is kept by the store"
        );
        assert!(!owners.owns_task("done", &owner("alice"), later));
    }

    #[tokio::test]
    async fn dropped_handler_marks_the_task_failed_so_it_can_be_swept() {
        let storage = InMemoryTaskStorage::new();
        let id = task_in(&storage, "abandoned", TaskState::TASK_STATE_WORKING).await;
        drop(FailOnDrop {
            storage: storage.clone(),
            task: Some(id.clone()),
        });
        for _ in 0..50 {
            tokio::task::yield_now().await;
            if storage
                .get(&id, None)
                .await
                .unwrap()
                .status
                .state
                .is_terminal()
            {
                break;
            }
        }
        let state = storage.get(&id, None).await.unwrap().status.state;
        assert!(state.is_terminal(), "{state:?}");

        let owners = Ownership::default();
        let now = Utc::now();
        owners.claim_task("abandoned", &owner("a"), now);
        sweep_once(
            &storage,
            &owners,
            Duration::from_secs(1),
            now + chrono::TimeDelta::seconds(5),
        )
        .await;
        assert!(!storage.exists(&id).await.unwrap());
    }

    #[tokio::test]
    async fn disarmed_guard_leaves_the_task_alone() {
        let storage = InMemoryTaskStorage::new();
        let id = task_in(&storage, "ok", TaskState::TASK_STATE_WORKING).await;
        let mut g = FailOnDrop {
            storage: storage.clone(),
            task: Some(id.clone()),
        };
        g.disarm();
        drop(g);
        tokio::task::yield_now().await;
        assert!(
            !storage
                .get(&id, None)
                .await
                .unwrap()
                .status
                .state
                .is_terminal()
        );
    }

    #[test]
    fn sweep_interval_is_a_quarter_of_the_ttl_capped_at_a_minute() {
        assert_eq!(
            sweep_interval(Duration::from_secs(900)),
            Duration::from_secs(60)
        );
        assert_eq!(
            sweep_interval(Duration::from_secs(40)),
            Duration::from_secs(10)
        );
        assert_eq!(
            sweep_interval(Duration::from_secs(2)),
            Duration::from_secs(1)
        );
    }

    #[test]
    fn authorize_enforces_the_allowlist_and_ownership() {
        let owners = Ownership::default();
        let now = Utc::now();
        let (alice, bob) = (owner("alice"), owner("bob"));
        let call =
            |method: &str, params: Value| json!({"id": 1, "method": method, "params": params});
        let send = |task: &str| call("SendMessage", json!({"message": {"taskId": task}}));

        authorize(&owners, &alice, &send("t1"), now).unwrap();
        assert!(matches!(
            authorize(&owners, &bob, &send("t1"), now),
            Err(A2AError::TaskNotFound(_))
        ));
        authorize(&owners, &alice, &call("GetTask", json!({"id": "t1"})), now).unwrap();
        assert!(matches!(
            authorize(&owners, &bob, &call("CancelTask", json!({"id": "t1"})), now),
            Err(A2AError::TaskNotFound(_))
        ));
        assert!(matches!(
            authorize(
                &owners,
                &bob,
                &call("GetTask", json!({"id": "never-seen"})),
                now
            ),
            Err(A2AError::TaskNotFound(_))
        ));
        for m in [
            "ListTasks",
            "CreateTaskPushNotificationConfig",
            "GetExtendedAgentCard",
        ] {
            assert!(matches!(
                authorize(&owners, &alice, &call(m, Value::Null), now),
                Err(A2AError::MethodNotFound(_))
            ));
        }
        authorize(&owners, &bob, &json!({"not": "a call"}), now).unwrap();
    }
}
