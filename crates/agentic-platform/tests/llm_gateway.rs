mod common;

use agentic_platform::prelude::*;
use axum::http::StatusCode;
use common::{CalcOut, Fixture, fixture, llm, mock_model, send, set_config, text, token};
use serde_json::{Value, json};
use wiremock::MockServer;

fn set_env(name: &str, value: &str) {
    // Each test owns distinct variable names, so concurrent tests never observe each other's writes.
    unsafe { std::env::set_var(name, value) };
}

fn gateway_fixture(url: &str, tag: &str, extra: &str) -> Fixture {
    let fx = fixture("reject", None, "all");
    set_config(
        &fx,
        "models: {default: \"mock:model\"}",
        &format!(
            "models: {{default: \"gateway:test-alias\"}}\n\
             llm_gateway: {{url: \"{url}\", api_key_env: GW_DEFAULT_{tag}, team_keys: {{docs-team: GW_DOCS_{tag}}}{extra}}}"
        ),
    );
    fx
}

async fn build(fx: &Fixture) -> agentic_platform::Result<Platform> {
    Platform::from_config(&fx.config)
        .unwrap()
        .jwt_keys(common::keys())
        .specialist(Specialist::<CalcOut>::new("calc_agent").description("Does arithmetic"))
        .build()
        .await
}

async fn run(app: &axum::Router, api_key: &str, input: &str) -> (StatusCode, Value) {
    let t = token(app, api_key).await;
    send(
        app,
        "POST",
        "/v1/agents/run",
        Some(&t),
        Some(json!({"input": input})),
    )
    .await
}

fn body(req: &wiremock::Request) -> Value {
    serde_json::from_slice(&req.body).unwrap()
}

fn auth(req: &wiremock::Request) -> &str {
    req.headers.get("authorization").unwrap().to_str().unwrap()
}

const ANSWER: &str = r#"{"answer": "five", "value": 5}"#;

#[tokio::test]
async fn gateway_spec_uses_team_key_and_sends_attribution() {
    let gw = llm(vec![text(ANSWER), text(ANSWER)]).await;
    set_env("GW_DEFAULT_A", "default-key");
    set_env("GW_DOCS_A", "docs-team-key");
    let fx = gateway_fixture(&gw.uri(), "A", "");
    let app = build(&fx).await.unwrap().router();

    let (s, v) = run(&app, "alice-key", "add sum").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (s, v) = run(&app, "root-key", "add sum").await;
    assert_eq!(s, StatusCode::OK, "{v}");

    let reqs = gw.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 2);
    assert_eq!(reqs[0].url.path(), "/v1/chat/completions");
    assert_eq!(auth(&reqs[0]), "Bearer docs-team-key");
    assert_eq!(auth(&reqs[1]), "Bearer default-key");

    let b = body(&reqs[0]);
    assert_eq!(b["model"], "test-alias");
    assert_eq!(b["user"], "alice");
    let m = &b["metadata"];
    assert_eq!(m["team"], "docs-team");
    assert_eq!(m["application"], "test-app");
    assert_eq!(m["agent"], "calc_agent");
    assert_eq!(m["use_case"], "calc_agent");
    assert_eq!(m["prompt_version"], "2.0.0");
    assert!(!m["request_id"].as_str().unwrap().is_empty());
    assert_eq!(
        m["tags"],
        json!(["team:docs-team", "app:test-app", "agent:calc_agent"])
    );
    assert_eq!(m["spend_logs_metadata"]["team"], "docs-team");
    assert_eq!(m["spend_logs_metadata"]["agent"], "calc_agent");
    assert_eq!(m["spend_logs_metadata"]["request_id"], m["request_id"]);
    assert_eq!(body(&reqs[1])["user"], "root");
    assert_eq!(body(&reqs[1])["metadata"]["team"], "platform-team");
}

#[tokio::test]
async fn request_id_in_metadata_matches_the_response() {
    let gw = llm(vec![text(ANSWER)]).await;
    set_env("GW_DEFAULT_B", "k");
    let fx = gateway_fixture(&gw.uri(), "B", "");
    let app = build(&fx).await.unwrap().router();
    let (s, v) = run(&app, "alice-key", "add sum").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let reqs = gw.received_requests().await.unwrap();
    assert_eq!(
        body(&reqs[0])["metadata"]["request_id"],
        v["request_id"],
        "{v}"
    );
}

#[tokio::test]
async fn send_metadata_false_and_trailing_v1_url() {
    let gw = llm(vec![text(ANSWER)]).await;
    set_env("GW_DEFAULT_C", "k");
    let fx = gateway_fixture(&format!("{}/v1/", gw.uri()), "C", ", send_metadata: false");
    let app = build(&fx).await.unwrap().router();
    let (s, v) = run(&app, "alice-key", "add sum").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let b = body(&gw.received_requests().await.unwrap()[0]);
    assert!(
        b.get("user").is_none() && b.get("metadata").is_none(),
        "{b}"
    );
}

#[tokio::test]
async fn coordinator_on_the_gateway_is_attributed_to_coordinator() {
    let gw = llm(vec![text(r#"{"agent": "calc_agent"}"#), text(ANSWER)]).await;
    set_env("GW_DEFAULT_D", "k");
    let fx = gateway_fixture(&gw.uri(), "D", "");
    set_config(&fx, "unclassified: reject", "unclassified: llm");
    let app = build(&fx).await.unwrap().router();
    let (s, v) = run(&app, "alice-key", "what is two plus two").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let reqs = gw.received_requests().await.unwrap();
    assert_eq!(body(&reqs[0])["metadata"]["agent"], "coordinator");
    assert_eq!(body(&reqs[0])["metadata"]["team"], "docs-team");
    assert_eq!(body(&reqs[1])["metadata"]["agent"], "calc_agent");
}

#[tokio::test]
async fn direct_specs_carry_no_attribution() {
    let direct: MockServer = llm(vec![text(ANSWER)]).await;
    let fx = fixture("reject", None, "all");
    set_config(
        &fx,
        "models: {default: \"mock:model\"}",
        "models: {default: \"openai:direct\"}\nllm_gateway: {url: \"http://127.0.0.1:9\", api_key_env: GW_DEFAULT_E}",
    );
    set_env("GW_DEFAULT_E", "k");
    let app = Platform::from_config(&fx.config)
        .unwrap()
        .jwt_keys(common::keys())
        .specialist(Specialist::<CalcOut>::new("calc_agent").description("d"))
        .model("openai:direct", mock_model(&format!("{}/v1", direct.uri())))
        .build()
        .await
        .unwrap()
        .router();
    let (s, v) = run(&app, "alice-key", "add sum").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let b = body(&direct.received_requests().await.unwrap()[0]);
    assert!(
        b.get("user").is_none() && b.get("metadata").is_none(),
        "{b}"
    );
}

#[tokio::test]
async fn gateway_spec_without_block_fails_the_build() {
    let fx = fixture("reject", None, "all");
    set_config(
        &fx,
        "models: {default: \"mock:model\"}",
        "models: {default: \"gateway:x\"}",
    );
    let e = build(&fx).await.err().expect("build must fail").to_string();
    assert!(
        e.contains("model spec `gateway:x` needs an `llm_gateway` block in platform.yaml"),
        "{e}"
    );
}

#[tokio::test]
async fn missing_default_key_fails_the_build_only_when_a_gateway_spec_is_used() {
    let fx = gateway_fixture("http://127.0.0.1:9", "F", "");
    let e = build(&fx).await.err().expect("build must fail").to_string();
    assert!(e.contains("GW_DEFAULT_F"), "{e}");

    set_config(
        &fx,
        "models: {default: \"gateway:test-alias\"}",
        "models: {default: \"mock:model\"}",
    );
    let unused = Platform::from_config(&fx.config)
        .unwrap()
        .jwt_keys(common::keys())
        .specialist(Specialist::<CalcOut>::new("calc_agent").description("d"))
        .model("mock:model", mock_model("http://127.0.0.1:9/v1"))
        .build()
        .await;
    assert!(unused.is_ok());
}

#[tokio::test]
async fn missing_team_key_falls_back_to_the_default_key() {
    let gw = llm(vec![text(ANSWER)]).await;
    set_env("GW_DEFAULT_G", "default-key");
    let fx = gateway_fixture(&gw.uri(), "G", "");
    let app = build(&fx).await.unwrap().router();
    let (s, v) = run(&app, "alice-key", "add sum").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(
        auth(&gw.received_requests().await.unwrap()[0]),
        "Bearer default-key"
    );
}
