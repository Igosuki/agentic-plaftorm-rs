use std::{sync::Arc, time::Duration};

use a2a_protocol_types::{AgentCapabilities, AgentSkill};
use adk_agent::LlmAgentBuilder;
use adk_core::{Llm, RunConfig, StreamingMode};
use adk_runner::Runner;
use adk_server::a2a::v1::{
    card::{CachedAgentCard, build_v1_agent_card},
    executor::V1Executor,
    jsonrpc_handler::jsonrpc_handler,
    push::NoOpPushNotificationSender,
    request_handler::RequestHandler,
    task_store::InMemoryTaskStore,
};
use adk_session::InMemorySessionService;
use axum::{
    Router,
    http::{StatusCode, header},
    response::IntoResponse,
    routing::{get, post},
};
use tokio::sync::RwLock;

use crate::{
    auth::{self, BearerAuth},
    retention, shim,
    tools::{GetRunbook, ListRunbooks},
};

pub const AGENT_NAME: &str = "ops_agent";
const DESCRIPTION: &str = "Incident and runbook assistant for the Nimbus Queue message queue.";

const INSTRUCTION: &str = r#"You are an incident assistant for the Nimbus Queue message queue.
Always call list_runbooks first, then get_runbook for the best matching id, before answering. Never answer from memory.
Reply with ONLY one JSON object, no prose and no code fences, exactly of this shape:
{"summary": string, "runbook_id": string|null, "severity": "low"|"medium"|"high", "steps": [string]}
Rules:
- runbook_id must be an id returned by the tools, or null if no runbook fits.
- severity is one of "low", "medium", "high"; use the runbook's severity unless the question states thresholds that justify escalating.
- steps are the concrete actions from the runbook, in order, with its commands and thresholds; use [] only when runbook_id is null.
- summary is one or two sentences.
- Output no keys other than these four."#;

pub fn build_card(public_url: &str) -> a2a_protocol_types::AgentCard {
    let skills = vec![AgentSkill {
        id: "ops_triage".into(),
        name: "Ops triage".into(),
        description: "Match a Nimbus Queue incident to a runbook and return structured remediation steps as JSON.".into(),
        tags: vec!["ops".into(), "incident".into(), "runbook".into()],
        examples: Some(vec!["Consumer lag on orders is growing, what do I do?".into()]),
        input_modes: None,
        output_modes: None,
        security_requirements: None,
    }];
    build_v1_agent_card(
        AGENT_NAME,
        DESCRIPTION,
        public_url,
        env!("CARGO_PKG_VERSION"),
        skills,
        AgentCapabilities::default(),
    )
}

pub async fn build_router(
    model: Arc<dyn Llm>,
    public_url: &str,
    bearer: Option<BearerAuth>,
    task_ttl: Duration,
) -> anyhow::Result<Router> {
    let agent = LlmAgentBuilder::new(AGENT_NAME)
        .description(DESCRIPTION)
        .instruction(INSTRUCTION)
        .model(model)
        .temperature(0.0)
        .max_iterations(8)
        .tool(Arc::new(ListRunbooks))
        .tool(Arc::new(GetRunbook))
        .build()?;
    let sessions = Arc::new(InMemorySessionService::new());
    // StreamingMode::None is required: with streaming the artifact text is emitted twice.
    let runner_cfg = Arc::new(
        Runner::builder()
            .app_name(AGENT_NAME)
            .agent(Arc::new(agent))
            .session_service(sessions.clone())
            .run_config(RunConfig {
                streaming_mode: StreamingMode::None,
                ..Default::default()
            })
            .build_config(),
    );
    let cached = Arc::new(RwLock::new(CachedAgentCard::new(build_card(public_url))));
    let store = Arc::new(InMemoryTaskStore::new());
    retention::spawn(&store, sessions, AGENT_NAME, task_ttl);
    let handler = Arc::new(RequestHandler::with_runner(
        Arc::new(V1Executor::new(store.clone())),
        store,
        Arc::new(NoOpPushNotificationSender),
        cached.clone(),
        runner_cfg,
    ));
    let card_json = cached.read().await.card_json.clone();

    let mut rpc = Router::new()
        .route("/", post(jsonrpc_handler))
        .with_state(handler)
        .layer(axum::middleware::from_fn(shim::middleware));
    if let Some(b) = bearer {
        rpc = rpc.layer(axum::middleware::from_fn_with_state(
            Arc::new(b),
            auth::middleware,
        ));
    }
    Ok(rpc
        .route(
            "/.well-known/agent-card.json",
            get(move || {
                let body = card_json.clone();
                async move { ([(header::CONTENT_TYPE, "application/json")], body).into_response() }
            }),
        )
        .route("/health", get(|| async { StatusCode::OK })))
}
