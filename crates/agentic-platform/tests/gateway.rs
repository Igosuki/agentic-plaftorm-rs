mod common;

use axum::http::StatusCode;
use common::{fixture, platform, send, set_config, token};
use serde_json::json;

const NO_LLM: &str = "http://127.0.0.1:9/v1";

#[tokio::test]
async fn health_and_metrics_are_open() {
    let fx = fixture("reject", None, "all");
    let app = platform(&fx, false, NO_LLM).await.router();
    let (s, v) = send(&app, "GET", "/healthz", None, None).await;
    assert_eq!((s, v["status"].as_str()), (StatusCode::OK, Some("ok")));
    let (s, _) = send(&app, "GET", "/metrics", None, None).await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn auth_token_flow() {
    let fx = fixture("reject", None, "all");
    let app = platform(&fx, false, NO_LLM).await.router();

    let (s, v) = send(
        &app,
        "POST",
        "/auth/token",
        None,
        Some(json!({"api_key": "nope"})),
    )
    .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(v["error"]["code"], "invalid_api_key");
    assert!(v["request_id"].as_str().is_some_and(|r| !r.is_empty()));

    let (s, v) = send(
        &app,
        "POST",
        "/auth/token",
        None,
        Some(json!({"api_key": "alice-key"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["token_type"], "Bearer");
    assert_eq!(v["expires_in"], 3600);

    let (s, v) = send(&app, "POST", "/auth/token", None, Some(json!({"wrong": 1}))).await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_request"))
    );
}

#[tokio::test]
async fn protected_routes_reject_bad_tokens() {
    let fx = fixture("reject", None, "all");
    let app = platform(&fx, false, NO_LLM).await.router();
    for (uri, m) in [
        ("/v1/agents", "GET"),
        ("/v1/metrics/me", "GET"),
        ("/admin/prompts", "GET"),
        ("/v1/agents/run", "POST"),
    ] {
        let (s, v) = send(&app, m, uri, None, Some(json!({"input": "x"}))).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "{uri}");
        assert_eq!(v["error"]["code"], "unauthorized");
        let (s, _) = send(&app, m, uri, Some("garbage"), Some(json!({"input": "x"}))).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "{uri}");
    }
    let other = agentic_core::auth::JwtKeys::from_secret(b"some-other-secret-some-other-secret")
        .issue_user_token("alice", "t", &[], "agentic-platform", 60)
        .unwrap();
    let (s, _) = send(&app, "GET", "/v1/agents", Some(&other), None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let mcp_aud = common::keys()
        .issue_service_token("agentic-platform", 60)
        .unwrap();
    let (s, _) = send(&app, "GET", "/v1/agents", Some(&mcp_aud), None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn lists_agents() {
    let fx = fixture("reject", None, "all");
    let app = platform(&fx, false, NO_LLM).await.router();
    let t = token(&app, "alice-key").await;
    let (s, v) = send(&app, "GET", "/v1/agents", Some(&t), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v[0]["name"], "calc_agent");
    assert_eq!(v[0]["prompt_id"], "calc_agent");
    assert_eq!(v[0]["description"], "Does arithmetic");
}

#[tokio::test]
async fn unclassified_is_422_with_scores() {
    let fx = fixture("reject", None, "all");
    let app = platform(&fx, false, NO_LLM).await.router();
    let t = token(&app, "alice-key").await;
    let (s, v) = send(
        &app,
        "POST",
        "/v1/agents/run",
        Some(&t),
        Some(json!({"input": "what is the weather"})),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(v["error"]["code"], "unclassified");
    assert_eq!(v["trace"]["classification"]["scores"]["calc_agent"], 0);
    assert!(v["request_id"].is_string());

    let (s, v) = send(
        &app,
        "POST",
        "/v1/agents/run",
        Some(&t),
        Some(json!({"input": "  "})),
    )
    .await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_request"))
    );

    let (_, me) = send(&app, "GET", "/v1/metrics/me", Some(&t), None).await;
    assert_eq!(
        (
            me["user_id"].as_str(),
            me["requests"].as_u64(),
            me["errors"].as_u64()
        ),
        (Some("alice"), Some(1), Some(1))
    );
}

#[tokio::test]
async fn admin_endpoints_require_admin_role() {
    let fx = fixture("reject", None, "all");
    let app = platform(&fx, false, NO_LLM).await.router();
    let reader = token(&app, "alice-key").await;
    let admin = token(&app, "root-key").await;

    for (m, uri, body) in [
        ("GET", "/admin/prompts", None),
        ("GET", "/admin/prompts/calc_agent", None),
        (
            "POST",
            "/admin/prompts/calc_agent/live",
            Some(json!({"version": "1.0.0"})),
        ),
        ("POST", "/admin/prompts/calc_agent/rollback", None),
        ("GET", "/v1/metrics/teams", None),
    ] {
        let (s, v) = send(&app, m, uri, Some(&reader), body).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{uri}");
        assert_eq!(v["error"]["code"], "forbidden");
    }

    let (s, v) = send(&app, "GET", "/admin/prompts", Some(&admin), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v[0]["id"], "calc_agent");
    assert_eq!(v[0]["live"], "2.0.0");
    assert_eq!(v[0]["versions"].as_array().unwrap().len(), 2);

    let (s, v) = send(&app, "GET", "/admin/prompts/calc_agent", Some(&admin), None).await;
    assert_eq!(
        (s, v["versions"][0]["template"].as_str()),
        (StatusCode::OK, Some("You are calc v1."))
    );
    let (s, _) = send(&app, "GET", "/admin/prompts/missing", Some(&admin), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn prompt_live_and_rollback() {
    let fx = fixture("reject", None, "all");
    let app = platform(&fx, false, NO_LLM).await.router();
    let admin = token(&app, "root-key").await;

    let (s, _) = send(
        &app,
        "POST",
        "/admin/prompts/calc_agent/rollback",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
    let (s, v) = send(
        &app,
        "POST",
        "/admin/prompts/calc_agent/live",
        Some(&admin),
        Some(json!({"version": "9"})),
    )
    .await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (StatusCode::NOT_FOUND, Some("version_not_found"))
    );

    let (s, v) = send(
        &app,
        "POST",
        "/admin/prompts/calc_agent/live",
        Some(&admin),
        Some(json!({"version": "1.0.0"})),
    )
    .await;
    assert_eq!((s, v["live"].as_str()), (StatusCode::OK, Some("1.0.0")));
    assert!(fx.dir.path().join("state/prompts.json").exists());

    let (s, v) = send(
        &app,
        "POST",
        "/admin/prompts/calc_agent/rollback",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!((s, v["live"].as_str()), (StatusCode::OK, Some("2.0.0")));
}

#[tokio::test]
async fn team_metrics_visible_to_admin() {
    let fx = fixture("reject", None, "all");
    let app = platform(&fx, false, NO_LLM).await.router();
    let reader = token(&app, "alice-key").await;
    let admin = token(&app, "root-key").await;
    send(
        &app,
        "POST",
        "/v1/agents/run",
        Some(&reader),
        Some(json!({"input": "weather"})),
    )
    .await;
    let (s, v) = send(&app, "GET", "/v1/metrics/teams", Some(&admin), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v[0]["team"], "docs-team");
    assert_eq!(v[0]["requests"], 1);
}

#[tokio::test]
async fn rate_limit_returns_429() {
    let fx = fixture("reject", None, "all");
    let cfg = std::fs::read_to_string(&fx.config).unwrap().replace(
        "requests_per_minute_per_team: 1000",
        "requests_per_minute_per_team: 1",
    );
    std::fs::write(&fx.config, cfg).unwrap();
    let app = platform(&fx, false, NO_LLM).await.router();
    let t = token(&app, "alice-key").await;
    let (s, _) = send(
        &app,
        "POST",
        "/v1/agents/run",
        Some(&t),
        Some(json!({"input": "weather"})),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    let (s, v) = send(
        &app,
        "POST",
        "/v1/agents/run",
        Some(&t),
        Some(json!({"input": "weather"})),
    )
    .await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (StatusCode::TOO_MANY_REQUESTS, Some("rate_limited"))
    );
}

async fn run(
    app: &axum::Router,
    t: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    send(app, "POST", "/v1/agents/run", Some(t), Some(body)).await
}

#[tokio::test]
async fn application_must_be_allowlisted() {
    let fx = fixture("reject", None, "all");
    set_config(
        &fx,
        "app: test-app\n",
        "app: test-app\napplications: [test-app, other-app]\n",
    );
    let app = platform(&fx, false, NO_LLM).await.router();
    let t = token(&app, "alice-key").await;

    let (s, v) = run(
        &app,
        &t,
        json!({"input": "weather", "application": "evil\nvalue"}),
    )
    .await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_request"))
    );
    assert!(!v["error"]["message"].as_str().unwrap().contains('\n'));
    let (s, _) = run(
        &app,
        &t,
        json!({"input": "weather", "application": "other-app"}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);

    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/v1/agents/run")
        .header("authorization", format!("Bearer {t}"))
        .header("content-type", "application/json")
        .header("x-application", "nope")
        .body(axum::body::Body::from(
            json!({"input": "weather"}).to_string(),
        ))
        .unwrap();
    let resp = tower::ServiceExt::oneshot(app.clone(), req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let (_, prom) = send(&app, "GET", "/metrics", None, None).await;
    assert!(!prom.as_str().unwrap().contains("evil"));
}

#[tokio::test]
async fn use_case_must_match_pattern_and_allowlist() {
    let fx = fixture("reject", None, "all");
    let app = platform(&fx, false, NO_LLM).await.router();
    let t = token(&app, "alice-key").await;
    for bad in ["Calc", "calc agent", "other_case", "", &"a".repeat(65)] {
        let (s, v) = run(&app, &t, json!({"input": "weather", "use_case": bad})).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{bad}: {v}");
    }
    let (s, _) = run(
        &app,
        &t,
        json!({"input": "weather", "use_case": "calc_agent"}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);

    set_config(
        &fx,
        "app: test-app\n",
        "app: test-app\nuse_cases: [reports, calc_agent]\n",
    );
    let app = platform(&fx, false, NO_LLM).await.router();
    let t = token(&app, "alice-key").await;
    let (s, _) = run(&app, &t, json!({"input": "weather", "use_case": "reports"})).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    let (s, _) = run(&app, &t, json!({"input": "weather", "use_case": "billing"})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn invalid_configured_use_case_fails_build() {
    let fx = fixture("reject", None, "all");
    set_config(
        &fx,
        "app: test-app\n",
        "app: test-app\nuse_cases: [\"Bad Case\"]\n",
    );
    let b = agentic_platform::prelude::Platform::from_config(&fx.config)
        .unwrap()
        .jwt_keys(common::keys())
        .specialist(agentic_platform::prelude::Specialist::<common::CalcOut>::new("calc_agent"))
        .model("mock:model", common::mock_model(NO_LLM));
    assert!(b.build().await.is_err());
}

#[tokio::test]
async fn unknown_engine_fails_build_listing_supported() {
    let fx = fixture("reject", None, "all");
    set_config(&fx, "app: test-app\n", "app: test-app\nengine: nope\n");
    let err = agentic_platform::prelude::Platform::from_config(&fx.config)
        .unwrap()
        .jwt_keys(common::keys())
        .specialist(agentic_platform::prelude::Specialist::<common::CalcOut>::new("calc_agent"))
        .model("mock:model", common::mock_model(NO_LLM))
        .build()
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(
        err.contains("`nope`") && err.contains("supported engines: ") && err.contains("rig"),
        "{err}"
    );
}

#[tokio::test]
async fn unknown_specialist_engine_fails_build() {
    let fx = fixture("reject", None, "all");
    let err = agentic_platform::prelude::Platform::from_config(&fx.config)
        .unwrap()
        .jwt_keys(common::keys())
        .specialist(
            agentic_platform::prelude::Specialist::<common::CalcOut>::new("calc_agent")
                .engine("nope"),
        )
        .model("mock:model", common::mock_model(NO_LLM))
        .build()
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(
        err.contains("calc_agent") && err.contains("`nope`"),
        "{err}"
    );
}

#[tokio::test]
async fn metrics_listen_moves_metrics_off_the_main_router() {
    let fx = fixture("reject", None, "all");
    set_config(
        &fx,
        "app: test-app\n",
        "app: test-app\nmetrics_listen: 127.0.0.1:0\n",
    );
    let p = platform(&fx, false, NO_LLM).await;
    let (main, metrics) = (p.router(), p.metrics_router());
    let (s, _) = send(&main, "GET", "/metrics", None, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = send(&main, "GET", "/healthz", None, None).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = send(&metrics, "GET", "/metrics", None, None).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = send(&metrics, "GET", "/healthz", None, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn auth_token_is_throttled_per_client_ip() {
    use axum::{body::Body, extract::ConnectInfo, http::Request};
    use std::net::SocketAddr;
    use tower::ServiceExt;

    let fx = fixture("reject", None, "all");
    set_config(
        &fx,
        "rate_limit: {requests_per_minute_per_team: 1000}",
        "rate_limit: {requests_per_minute_per_team: 1000, auth_rate_limit_per_minute: 2}",
    );
    let app = platform(&fx, false, NO_LLM).await.router();
    let from = |ip: &str| {
        let mut req = Request::builder()
            .method("POST")
            .uri("/auth/token")
            .header("content-type", "application/json")
            .body(Body::from(json!({"api_key": "alice-key"}).to_string()))
            .unwrap();
        let addr: SocketAddr = format!("{ip}:5000").parse().unwrap();
        req.extensions_mut().insert(ConnectInfo(addr));
        req
    };
    let status = |ip: &'static str| {
        let app = app.clone();
        async move { app.oneshot(from(ip)).await.unwrap().status() }
    };
    assert_eq!(status("10.0.0.1").await, StatusCode::OK);
    assert_eq!(status("10.0.0.1").await, StatusCode::OK);
    assert_eq!(status("10.0.0.1").await, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(status("10.0.0.2").await, StatusCode::OK);
}

#[tokio::test]
async fn auth_token_without_connect_info_shares_one_bucket() {
    let fx = fixture("reject", None, "all");
    set_config(
        &fx,
        "rate_limit: {requests_per_minute_per_team: 1000}",
        "rate_limit: {requests_per_minute_per_team: 1000, auth_rate_limit_per_minute: 1}",
    );
    let app = platform(&fx, false, NO_LLM).await.router();
    let body = || Some(json!({"api_key": "alice-key"}));
    assert_eq!(
        send(&app, "POST", "/auth/token", None, body()).await.0,
        StatusCode::OK
    );
    let (s, v) = send(&app, "POST", "/auth/token", None, body()).await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (StatusCode::TOO_MANY_REQUESTS, Some("rate_limited"))
    );
}

#[tokio::test]
async fn untracked_live_change_keeps_rollback_history_clean() {
    let fx = fixture("reject", None, "all");
    let app = platform(&fx, false, NO_LLM).await.router();
    let admin = token(&app, "root-key").await;
    let live = |v: &str| Some(json!({"version": v, "record_history": false}));
    for v in ["1.0.0", "2.0.0"] {
        let uri = "/admin/prompts/calc_agent/live";
        let (s, _) = send(&app, "POST", uri, Some(&admin), live(v)).await;
        assert_eq!(s, StatusCode::OK);
    }
    let (s, _) = send(
        &app,
        "POST",
        "/admin/prompts/calc_agent/rollback",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
}
