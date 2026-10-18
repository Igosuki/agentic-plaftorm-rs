mod common;

use agentic_core::auth::{AUD_GATEWAY, AUD_MCP};
use agentic_platform::pipeline::{RunRequest, authenticate, run_pipeline};
use axum::http::StatusCode;
use common::{fixture, llm, platform, text, token};
use serde_json::json;

const NO_LLM: &str = "http://127.0.0.1:9/v1";

fn req(input: &str, hint: Option<&str>) -> RunRequest {
    RunRequest {
        input: input.into(),
        application: None,
        use_case: None,
        agent_hint: hint.map(str::to_owned),
    }
}

#[tokio::test]
async fn authenticate_checks_audience_and_scheme() {
    let fx = fixture("reject", None, "all");
    let p = platform(&fx, false, NO_LLM).await;
    let t = token(&p.router(), "alice-key").await;
    let st = p.state();

    let ctx = authenticate(st, Some(&format!("Bearer {t}")), AUD_GATEWAY).unwrap();
    assert_eq!(
        (ctx.user.as_str(), ctx.team.as_str()),
        ("alice", "docs-team")
    );
    assert_eq!(ctx.application, "test-app");

    for (hdr, aud) in [
        (None, AUD_GATEWAY),
        (Some(t.clone()), AUD_GATEWAY),
        (Some(format!("Bearer {t}")), AUD_MCP),
    ] {
        let e = authenticate(st, hdr.as_deref(), aud).unwrap_err();
        assert_eq!(
            (e.status(), e.code()),
            (StatusCode::UNAUTHORIZED, "unauthorized")
        );
        assert!(!e.request_id().is_empty());
    }
}

#[tokio::test]
async fn agent_hint_bypasses_the_intent_gate() {
    let model = llm(vec![text(r#"{"answer": "five", "value": 5}"#)]).await;
    let fx = fixture("reject", None, "all");
    let p = platform(&fx, false, &format!("{}/v1", model.uri())).await;
    let t = token(&p.router(), "alice-key").await;
    let ctx = authenticate(p.state(), Some(&format!("Bearer {t}")), AUD_GATEWAY).unwrap();

    let e = run_pipeline(p.state(), &ctx, req("what is the weather", None))
        .await
        .unwrap_err();
    assert_eq!(
        (e.status(), e.code()),
        (StatusCode::UNPROCESSABLE_ENTITY, "unclassified")
    );
    assert!(e.trace().is_some());

    let r = run_pipeline(
        p.state(),
        &ctx,
        req("what is the weather", Some("calc_agent")),
    )
    .await
    .unwrap();
    assert_eq!(r.agent, "calc_agent");
    assert_eq!(r.output, json!({"answer": "five", "value": 5}));
    assert_eq!(r.trace["classification"]["method"], "explicit");
    assert_eq!(r.request_id, ctx.request_id);
}

#[tokio::test]
async fn unknown_agent_hint_is_invalid_request() {
    let fx = fixture("reject", None, "all");
    let p = platform(&fx, false, NO_LLM).await;
    let t = token(&p.router(), "alice-key").await;
    let ctx = authenticate(p.state(), Some(&format!("Bearer {t}")), AUD_GATEWAY).unwrap();

    let e = run_pipeline(p.state(), &ctx, req("add 1 and 2", Some("nope\nagent")))
        .await
        .unwrap_err();
    assert_eq!(
        (e.status(), e.code()),
        (StatusCode::BAD_REQUEST, "invalid_request")
    );
    assert!(!e.message().contains('\n'));
}
