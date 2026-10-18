#![allow(dead_code)]

use std::path::PathBuf;

use agentic_core::auth::JwtKeys;
use agentic_platform::prelude::*;
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_partial_json, method, path},
};

pub const SECRET: &[u8] = b"integration-test-secret-integration";

pub fn keys() -> JwtKeys {
    JwtKeys::from_secret(SECRET)
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct CalcOut {
    pub answer: String,
    pub value: i64,
}

impl AgentOutput for CalcOut {
    fn validate(&self, evidence: &Evidence) -> Result<(), Violation> {
        if evidence.calls().is_empty() || evidence.mentions(&self.value.to_string()) {
            Ok(())
        } else {
            Err(Violation::Hallucination(format!(
                "{} does not appear in any tool result",
                self.value
            )))
        }
    }
}

pub struct Fixture {
    pub dir: tempfile::TempDir,
    pub config: PathBuf,
}

pub fn fixture(routing: &str, mcp_url: Option<&str>, visibility: &str) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    let mcp = mcp_url.map_or(String::new(), |u| {
        format!("mcp_servers:\n  calc: {{url: \"{u}\"}}\n")
    });
    std::fs::write(
        p.join("platform.yaml"),
        format!(
            "app: test-app\nmodels: {{default: \"mock:model\"}}\npricing: {{\"mock:model\": {{input: 1.0, output: 2.0}}}}\n\
             guard: {{max_attempts: 4, max_total_tokens: 100000, backoff_base_ms: 1}}\nrouting: {{unclassified: {routing}}}\n\
             tool_visibility: {visibility}\nprompt_state_file: ./state/prompts.json\nrate_limit: {{requests_per_minute_per_team: 1000}}\n{mcp}"
        ),
    )
    .unwrap();
    std::fs::write(
        p.join("users.yaml"),
        "users:\n  - {id: alice, api_key: alice-key, team: docs-team, roles: [reader]}\n  - {id: root, api_key: root-key, team: platform-team, roles: [admin]}\n",
    )
    .unwrap();
    std::fs::write(
        p.join("intents.yaml"),
        "min_score: 2\nmin_margin: 1\nagents:\n  calc_agent:\n    keywords: {add: 2, sum: 2, calculate: 2}\n",
    )
    .unwrap();
    std::fs::write(
        p.join("policies.yaml"),
        "default: deny\nroles:\n  reader: {allow: [\"calc.add\"]}\n  admin: {allow: [\"*\"]}\n",
    )
    .unwrap();
    std::fs::create_dir(p.join("prompts")).unwrap();
    std::fs::write(
        p.join("prompts/calc_agent.yaml"),
        "id: calc_agent\nlive: 2.0.0\nversions:\n  - {version: 1.0.0, template: \"You are calc v1.\"}\n  - {version: 2.0.0, template: \"You are calc v2.\", temperature: 0.1}\n",
    )
    .unwrap();
    Fixture {
        config: p.join("platform.yaml"),
        dir,
    }
}

pub async fn platform(fx: &Fixture, with_mcp: bool, model_base_url: &str) -> Platform {
    platform_with(fx, with_mcp, model_base_url, |s| s).await
}

pub async fn platform_with(
    fx: &Fixture,
    with_mcp: bool,
    model_base_url: &str,
    tweak: impl FnOnce(Specialist<CalcOut>) -> Specialist<CalcOut>,
) -> Platform {
    let mut spec = tweak(Specialist::<CalcOut>::new("calc_agent").description("Does arithmetic"));
    if with_mcp {
        spec = spec.mcp_server("calc");
    }
    let b = Platform::from_config(&fx.config)
        .unwrap()
        .jwt_keys(keys())
        .specialist(spec);
    let b = b.model("mock:model", mock_model(model_base_url));
    b.build().await.unwrap()
}

pub fn set_config(fx: &Fixture, from: &str, to: &str) {
    let cfg = std::fs::read_to_string(&fx.config).unwrap();
    assert!(cfg.contains(from), "{from} not in config");
    std::fs::write(&fx.config, cfg.replace(from, to)).unwrap();
}

pub async fn send(
    app: &Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let body = match body {
        Some(b) => {
            req = req.header("content-type", "application/json");
            Body::from(b.to_string())
        }
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    (status, value)
}

pub async fn token(app: &Router, api_key: &str) -> String {
    let (s, v) = send(
        app,
        "POST",
        "/auth/token",
        None,
        Some(json!({"api_key": api_key})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    v["access_token"].as_str().unwrap().to_owned()
}

pub fn mock_model(base_url: &str) -> rig::DynModel<rig::operation::Completion> {
    use rig::providers::openai::OpenAIConfig;
    let mut cfg = OpenAIConfig::new("test-key");
    cfg.base_url = base_url.to_owned();
    cfg.client().chat("mock-model").erase()
}

/// Replies with a fixed JSON-RPC body carrying the id of the request it answers.
pub struct EchoId(Value);

impl wiremock::Respond for EchoId {
    fn respond(&self, req: &wiremock::Request) -> ResponseTemplate {
        let mut v = self.0.clone();
        v["id"] = serde_json::from_slice::<Value>(&req.body)
            .ok()
            .and_then(|b| b.get("id").cloned())
            .unwrap_or(Value::Null);
        ResponseTemplate::new(200).set_body_json(v)
    }
}

pub fn completion(message: Value, finish: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "id": "cmpl-1", "object": "chat.completion", "created": 0, "model": "mock-model",
        "choices": [{"index": 0, "message": message, "finish_reason": finish}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
    }))
}

pub fn text(content: &str) -> ResponseTemplate {
    completion(json!({"role": "assistant", "content": content}), "stop")
}

pub fn tool_call(name: &str, args: &str) -> ResponseTemplate {
    completion(
        json!({"role": "assistant", "content": null, "tool_calls": [
            {"id": "call_1", "type": "function", "function": {"name": name, "arguments": args}}
        ]}),
        "tool_calls",
    )
}

pub async fn llm(responses: Vec<ResponseTemplate>) -> MockServer {
    let server = MockServer::start().await;
    for r in responses {
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(r)
            .up_to_n_times(1)
            .mount(&server)
            .await;
    }
    server
}

pub async fn mcp(call_reply: Value) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "tools/list"})))
        .respond_with(EchoId(json!({
            "jsonrpc": "2.0",
            "result": {"tools": [{"name": "add", "description": "Add two integers", "inputSchema": {
                "type": "object", "properties": {"a": {"type": "integer"}, "b": {"type": "integer"}}, "required": ["a", "b"]}}]}
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "tools/call", "params": {"name": "add", "arguments": {"a": 2, "b": 3}}})))
        .respond_with(EchoId(call_reply))
        .mount(&server)
        .await;
    server
}

pub fn bearer(req: &wiremock::Request) -> String {
    req.headers
        .get("authorization")
        .unwrap()
        .to_str()
        .unwrap()
        .trim_start_matches("Bearer ")
        .to_owned()
}
