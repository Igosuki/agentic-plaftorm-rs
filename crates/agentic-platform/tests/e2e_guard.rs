mod common;

use axum::http::StatusCode;
use common::{bearer, fixture, llm, mcp, platform, platform_with, send, text, token, tool_call};
use serde_json::{Value, json};
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

#[tokio::test]
async fn schema_violation_then_hallucination_then_ok_with_tool_call() {
    let llm = llm(vec![
        tool_call("add", r#"{"a":2,"b":3}"#),
        text("I think the answer is five"),
        text("```json\n{\"answer\": \"it is 99\", \"value\": 99}\n```"),
        text("Sure: {\"answer\": \"2 + 3 = 5\", \"value\": 5}"),
    ])
    .await;
    let mcp = mcp(json!({"jsonrpc": "2.0", "id": 1, "result": {"content": [{"type": "text", "text": "5"}], "resultType": "complete"}})).await;
    let fx = fixture("reject", Some(&format!("{}/mcp", mcp.uri())), "all");
    let app = platform(&fx, true, &format!("{}/v1", llm.uri()))
        .await
        .router();
    let t = token(&app, "alice-key").await;

    let (s, v) = send(
        &app,
        "POST",
        "/v1/agents/run",
        Some(&t),
        Some(json!({"input": "please add 2 and 3 and calculate the sum"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["agent"], "calc_agent");
    assert_eq!(v["prompt"], json!({"id": "calc_agent", "version": "2.0.0"}));
    assert_eq!(v["output"], json!({"answer": "2 + 3 = 5", "value": 5}));

    let outcomes: Vec<&str> = v["trace"]["attempts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["outcome"].as_str().unwrap())
        .collect();
    assert_eq!(outcomes, ["schema_violation", "hallucination", "ok"]);
    assert_eq!(v["trace"]["classification"]["method"], "keywords");
    assert_eq!(v["trace"]["engine"], "rig");
    assert_eq!(v["trace"]["tool_calls"].as_array().unwrap().len(), 1);
    assert_eq!(v["trace"]["tool_calls"][0]["outcome"], "ok");
    assert_eq!(v["trace"]["tool_calls"][0]["server"], "calc");
    assert_eq!(v["trace"]["usage"]["input_tokens"], 40);
    assert_eq!(v["trace"]["usage"]["output_tokens"], 20);
    let cost = v["trace"]["usage"]["cost_usd"].as_f64().unwrap();
    assert!((cost - (40.0 * 1.0 + 20.0 * 2.0) / 1e6).abs() < 1e-12);

    let llm_requests = llm.received_requests().await.unwrap();
    assert_eq!(llm_requests.len(), 4);
    let last: Value = serde_json::from_slice(&llm_requests[3].body).unwrap();
    let transcript = last["messages"].to_string();
    assert!(
        transcript.contains("it is 99"),
        "retry must carry the prior answer"
    );
    assert!(transcript.contains("failed grounding checks"));
    assert!(transcript.contains("You are calc v2."));
    assert!(transcript.contains("matching this JSON Schema"));
    assert_eq!(last["tools"][0]["function"]["name"], "add");

    let calls: Vec<_> = mcp
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| String::from_utf8_lossy(&r.body).contains("tools/call"))
        .collect();
    assert_eq!(calls.len(), 1);
    let claims = common::keys()
        .verify(&bearer(&calls[0]), "mcp", "agentic-platform")
        .unwrap();
    assert_eq!(
        (claims.sub.as_str(), claims.team.as_str()),
        ("alice", "docs-team")
    );
    assert_eq!(claims.roles, ["reader"]);
    assert_eq!(claims.request_id.as_deref(), v["request_id"].as_str());
    assert!(claims.exp - claims.iat <= 60);

    let listing: Vec<_> = mcp
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| String::from_utf8_lossy(&r.body).contains("tools/list"))
        .collect();
    let svc = common::keys()
        .verify(&bearer(&listing[0]), "mcp", "agentic-platform")
        .unwrap();
    assert_eq!(svc.roles, ["platform"]);

    let (_, me) = send(&app, "GET", "/v1/metrics/me", Some(&t), None).await;
    assert_eq!(
        (
            me["successes"].as_u64(),
            me["hallucinations"].as_u64(),
            me["schema_violations"].as_u64()
        ),
        (Some(1), Some(1), Some(1))
    );

    let (_, prom) = send(&app, "GET", "/metrics", None, None).await;
    let prom = prom.as_str().unwrap();
    for needle in [
        "agentic_requests_total{",
        "agentic_attempts_total{",
        "outcome=\"hallucination\"",
        "agentic_tool_calls_total{",
        "agentic_tokens_total{",
        "agentic_request_duration_seconds_bucket{",
        "agentic_intent_total{",
        "prompt_version=\"2.0.0\"",
    ] {
        assert!(prom.contains(needle), "missing {needle} in\n{prom}");
    }
}

#[tokio::test]
async fn exhausted_retries_return_502_with_trace() {
    let llm = llm(vec![
        text("nope"),
        text("still nope"),
        text("never"),
        text("again"),
    ])
    .await;
    let fx = fixture("reject", None, "all");
    let app = platform(&fx, false, &format!("{}/v1", llm.uri()))
        .await
        .router();
    let t = token(&app, "alice-key").await;
    let (s, v) = send(
        &app,
        "POST",
        "/v1/agents/run",
        Some(&t),
        Some(json!({"input": "add things"})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_GATEWAY);
    assert_eq!(v["error"]["code"], "agent_failed");
    assert_eq!(v["trace"]["attempts"].as_array().unwrap().len(), 4);
    assert!(v["request_id"].is_string());
}

#[tokio::test]
async fn infra_error_is_retried_with_same_prompt() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(text(r#"{"answer":"hi","value":1}"#))
        .mount(&server)
        .await;
    let fx = fixture("reject", None, "all");
    let app = platform(&fx, false, &format!("{}/v1", server.uri()))
        .await
        .router();
    let t = token(&app, "alice-key").await;
    let (s, v) = send(
        &app,
        "POST",
        "/v1/agents/run",
        Some(&t),
        Some(json!({"input": "please calculate"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let outcomes: Vec<&str> = v["trace"]["attempts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["outcome"].as_str().unwrap())
        .collect();
    assert!(outcomes.last() == Some(&"ok"), "{outcomes:?}");
}

#[tokio::test]
async fn denied_tool_call_is_reported_not_fatal() {
    let llm = llm(vec![
        tool_call("add", r#"{"a":2,"b":3}"#),
        text(r#"{"answer":"I was not allowed to compute that","value":0}"#),
    ])
    .await;
    let mcp = mcp(json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32003, "message": "policy_denied: reader cannot call calc.add"}})).await;
    let fx = fixture("reject", Some(&format!("{}/mcp", mcp.uri())), "all");
    let app = platform(&fx, true, &format!("{}/v1", llm.uri()))
        .await
        .router();
    let t = token(&app, "alice-key").await;
    let (s, v) = send(
        &app,
        "POST",
        "/v1/agents/run",
        Some(&t),
        Some(json!({"input": "add 2 and 3 sum"})),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::BAD_GATEWAY,
        "validate() rejects ungrounded value 0 because a call exists: {v}"
    );
    assert_eq!(v["trace"]["tool_calls"][0]["outcome"], "denied");
    let (_, prom) = send(&app, "GET", "/metrics", None, None).await;
    assert!(prom.as_str().unwrap().contains("outcome=\"denied\""));

    let tool_msg: Vec<Value> = llm
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert!(
        tool_msg[1]["messages"]
            .to_string()
            .contains("denied by policy")
    );
}

#[tokio::test]
async fn filtered_visibility_hides_uncallable_tools() {
    let llm = llm(vec![text(r#"{"answer":"x","value":1}"#)]).await;
    let mcp = mcp(
        json!({"jsonrpc": "2.0", "id": 1, "result": {"content": [{"type": "text", "text": "5"}]}}),
    )
    .await;
    let fx = fixture("reject", Some(&format!("{}/mcp", mcp.uri())), "filtered");
    std::fs::write(
        fx.dir.path().join("policies.yaml"),
        "default: deny\nroles:\n  reader: {allow: [\"other.*\"]}\n  admin: {allow: [\"*\"]}\n",
    )
    .unwrap();
    let app = platform(&fx, true, &format!("{}/v1", llm.uri()))
        .await
        .router();
    let t = token(&app, "alice-key").await;
    let (s, v) = send(
        &app,
        "POST",
        "/v1/agents/run",
        Some(&t),
        Some(json!({"input": "add sum"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let req: Value =
        serde_json::from_slice(&llm.received_requests().await.unwrap()[0].body).unwrap();
    assert!(
        req.get("tools")
            .is_none_or(|t| t.as_array().is_none_or(Vec::is_empty)),
        "{req}"
    );
}

#[tokio::test]
async fn coordinator_llm_fallback_validates_answer() {
    let llm = llm(vec![
        text(r#"{"agent": "ghost"}"#),
        text(
            r#"```json
{"agent": "calc_agent"}
```"#,
        ),
        text(r#"{"answer":"four","value":4}"#),
    ])
    .await;
    let fx = fixture("llm", None, "all");
    let app = platform(&fx, false, &format!("{}/v1", llm.uri()))
        .await
        .router();
    let t = token(&app, "alice-key").await;

    let (s, v) = send(
        &app,
        "POST",
        "/v1/agents/run",
        Some(&t),
        Some(json!({"input": "what is two plus two"})),
    )
    .await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (StatusCode::UNPROCESSABLE_ENTITY, Some("unclassified"))
    );

    let (s, v) = send(
        &app,
        "POST",
        "/v1/agents/run",
        Some(&t),
        Some(json!({"input": "what is two plus two"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["trace"]["classification"]["method"], "llm");
    assert_eq!(v["output"]["value"], 4);
}

#[tokio::test]
async fn max_turns_is_its_own_outcome_with_nudge_and_counts_failed_usage() {
    let llm = llm(vec![
        tool_call("add", r#"{"a":2,"b":3}"#),
        text(r#"{"answer":"2 + 3 = 5","value":5}"#),
    ])
    .await;
    let mcp = mcp(json!({"jsonrpc": "2.0", "result": {"content": [{"type": "text", "text": "5"}], "resultType": "complete"}})).await;
    let fx = fixture("reject", Some(&format!("{}/mcp", mcp.uri())), "all");
    let app = platform_with(&fx, true, &format!("{}/v1", llm.uri()), |s| s.max_turns(1))
        .await
        .router();
    let t = token(&app, "alice-key").await;
    let (s, v) = send(
        &app,
        "POST",
        "/v1/agents/run",
        Some(&t),
        Some(json!({"input": "add 2 and 3 sum"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let outcomes: Vec<&str> = v["trace"]["attempts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["outcome"].as_str().unwrap())
        .collect();
    assert_eq!(outcomes, ["max_turns", "ok"]);
    assert_eq!(v["trace"]["usage"]["input_tokens"], 20);
    assert_eq!(v["trace"]["usage"]["output_tokens"], 10);

    let reqs = llm.received_requests().await.unwrap();
    let nudge: Value = serde_json::from_slice(&reqs.last().unwrap().body).unwrap();
    assert!(nudge["messages"].to_string().contains("Stop calling tools"));

    let (_, prom) = send(&app, "GET", "/metrics", None, None).await;
    let prom = prom.as_str().unwrap();
    assert!(prom.contains("outcome=\"max_turns\""), "{prom}");
}

#[tokio::test]
async fn failed_attempt_tokens_count_toward_the_budget() {
    let llm = llm(vec![
        tool_call("add", r#"{"a":2,"b":3}"#),
        text("never json"),
    ])
    .await;
    let mcp = mcp(json!({"jsonrpc": "2.0", "result": {"content": [{"type": "text", "text": "5"}], "resultType": "complete"}})).await;
    let fx = fixture("reject", Some(&format!("{}/mcp", mcp.uri())), "all");
    common::set_config(&fx, "max_total_tokens: 100000", "max_total_tokens: 10");
    let app = platform_with(&fx, true, &format!("{}/v1", llm.uri()), |s| s.max_turns(1))
        .await
        .router();
    let t = token(&app, "alice-key").await;
    let (s, v) = send(
        &app,
        "POST",
        "/v1/agents/run",
        Some(&t),
        Some(json!({"input": "add 2 and 3 sum"})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_GATEWAY, "{v}");
    assert_eq!(v["trace"]["attempts"].as_array().unwrap().len(), 1, "{v}");
    assert_eq!(v["trace"]["attempts"][0]["outcome"], "max_turns");
    assert_eq!(v["trace"]["usage"]["input_tokens"], 10);
    assert_eq!(llm.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn coordinator_usage_is_metered_under_its_own_agent_label() {
    let llm = llm(vec![
        text(r#"{"agent": "calc_agent"}"#),
        text(r#"{"answer":"four","value":4}"#),
    ])
    .await;
    let fx = fixture("llm", None, "all");
    let app = platform(&fx, false, &format!("{}/v1", llm.uri()))
        .await
        .router();
    let t = token(&app, "alice-key").await;
    let (s, v) = send(
        &app,
        "POST",
        "/v1/agents/run",
        Some(&t),
        Some(json!({"input": "what is two plus two"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["trace"]["usage"]["input_tokens"], 10);

    let (_, me) = send(&app, "GET", "/v1/metrics/me", Some(&t), None).await;
    assert_eq!(me["input_tokens"], 20);
    assert_eq!(me["output_tokens"], 10);
    assert_eq!(me["requests"], 1);

    let (_, prom) = send(&app, "GET", "/metrics", None, None).await;
    let prom = prom.as_str().unwrap();
    assert!(
        prom.lines().any(|l| l.starts_with("agentic_tokens_total{")
            && l.contains("agent=\"coordinator\"")
            && l.contains("direction=\"input\"")),
        "{prom}"
    );
    assert!(
        prom.lines().any(
            |l| l.starts_with("agentic_cost_usd_total{") && l.contains("agent=\"coordinator\"")
        ),
        "{prom}"
    );

    let admin = token(&app, "root-key").await;
    let (_, teams) = send(&app, "GET", "/v1/metrics/teams", Some(&admin), None).await;
    let total: u64 = teams
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["input_tokens"].as_u64().unwrap())
        .sum();
    assert_eq!(total, 20);
}

#[tokio::test]
async fn unresolvable_model_is_recorded_as_internal_error() {
    let fx = fixture("reject", None, "all");
    std::fs::write(
        fx.dir.path().join("prompts/calc_agent.yaml"),
        "id: calc_agent\nlive: 1.0.0\nversions:\n  - {version: 1.0.0, model: \"no-provider-colon\", template: \"t\"}\n",
    )
    .unwrap();
    let app = platform(&fx, false, "http://127.0.0.1:9/v1").await.router();
    let t = token(&app, "alice-key").await;
    let (s, v) = send(
        &app,
        "POST",
        "/v1/agents/run",
        Some(&t),
        Some(json!({"input": "add sum"})),
    )
    .await;
    assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR, "{v}");
    let (_, prom) = send(&app, "GET", "/metrics", None, None).await;
    assert!(
        prom.as_str()
            .unwrap()
            .contains("outcome=\"internal_error\""),
        "{prom}"
    );
    let (_, me) = send(&app, "GET", "/v1/metrics/me", Some(&t), None).await;
    assert_eq!(
        (me["requests"].as_u64(), me["errors"].as_u64()),
        (Some(1), Some(1))
    );
}
