mod common;

use axum::http::StatusCode;
use common::{fixture, llm, platform, send, text, token};
use serde_json::{Value, json};

const REQUEST: &str = "please add 2 and 3 and calculate the sum";

async fn run(app: &axum::Router, body: Value) -> (StatusCode, Value) {
    let t = token(app, "alice-key").await;
    send(app, "POST", "/v1/agents/run", Some(&t), Some(body)).await
}

#[tokio::test]
async fn ui_is_absent_unless_requested() {
    let llm = llm(vec![text(r#"{"answer": "5", "value": 5}"#)]).await;
    let fx = fixture("reject", None, "all");
    let app = platform(&fx, false, &format!("{}/v1", llm.uri()))
        .await
        .router();

    let (s, v) = run(&app, json!({"input": REQUEST})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert!(v.get("ui").is_none());
    assert!(v["trace"].get("display").is_none());
    assert_eq!(llm.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn unknown_ui_format_is_a_bad_request() {
    let fx = fixture("reject", None, "all");
    let app = platform(&fx, false, "http://127.0.0.1:9/v1").await.router();
    let (s, _) = run(&app, json!({"input": REQUEST, "ui": "html"})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[cfg(not(feature = "ui"))]
#[tokio::test]
async fn without_the_feature_ui_is_unavailable_and_visible() {
    let llm = llm(vec![text(r#"{"answer": "5", "value": 5}"#)]).await;
    let fx = fixture("reject", None, "all");
    let app = platform(&fx, false, &format!("{}/v1", llm.uri()))
        .await
        .router();

    let (s, v) = run(&app, json!({"input": REQUEST, "ui": "a2ui"})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["output"], json!({"answer": "5", "value": 5}));
    assert!(v["ui"].is_null());
    assert_eq!(v["trace"]["display"]["status"], "unavailable");
    assert!(
        v["trace"]["display"]["reason"]
            .as_str()
            .unwrap()
            .contains("`ui` cargo feature")
    );
    assert_eq!(llm.received_requests().await.unwrap().len(), 1);
}

#[cfg(feature = "ui")]
mod ui {
    use common::{Fixture, set_config};

    use super::*;

    fn with_display_prompt(fx: &Fixture) {
        std::fs::write(
            fx.dir.path().join("prompts/display_agent.yaml"),
            "id: display_agent\nlive: 1.0.0\nversions:\n  - {version: 1.0.0, template: \"You lay out results.\"}\n",
        )
        .unwrap();
    }

    fn layout(path: &str) -> String {
        json!({"components": [
            {"id": "root", "component": "Column", "children": ["answer"]},
            {"id": "answer", "component": "Text", "text": {"path": path}}
        ]})
        .to_string()
    }

    #[tokio::test]
    async fn renders_a_surface_bound_to_the_validated_output() {
        let llm = llm(vec![
            text(r#"{"answer": "2 + 3 = 5", "value": 5}"#),
            text(&layout("/answer")),
        ])
        .await;
        let fx = fixture("reject", None, "all");
        with_display_prompt(&fx);
        let app = platform(&fx, false, &format!("{}/v1", llm.uri()))
            .await
            .router();

        let (s, v) = run(&app, json!({"input": REQUEST, "ui": "a2ui"})).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        println!(
            "{}",
            serde_json::to_string_pretty(&v["trace"]["display"]).unwrap()
        );
        assert_eq!(v["output"], json!({"answer": "2 + 3 = 5", "value": 5}));

        let ui = &v["ui"];
        assert_eq!(
            (ui["protocol"].as_str(), ui["version"].as_str()),
            (Some("a2ui"), Some("0.9"))
        );
        let sid = ui["surface_id"].as_str().unwrap();
        let m = ui["messages"].as_array().unwrap();
        assert_eq!(m.len(), 3);
        assert_eq!(m[0]["createSurface"]["surfaceId"], sid);
        assert_eq!(m[1]["updateDataModel"]["value"], v["output"]);
        assert_eq!(
            m[2]["updateComponents"]["components"][1]["text"]["path"],
            "/answer"
        );

        let d = &v["trace"]["display"];
        assert_eq!(
            (d["status"].as_str(), d["attempts"].as_u64()),
            (Some("rendered"), Some(1))
        );
        assert!(d["reason"].is_null());
        assert_eq!(
            d["prompt"],
            json!({"id": "display_agent", "version": "1.0.0"})
        );
        assert_eq!(d["usage"]["input_tokens"], 10);
        assert_eq!(d["usage"]["output_tokens"], 5);
        assert_eq!(v["trace"]["usage"]["input_tokens"], 10);

        let (_, prom) = send(&app, "GET", "/metrics", None, None).await;
        assert!(prom.as_str().unwrap().contains("agent=\"display_agent\""));
    }

    #[tokio::test]
    async fn failed_layout_keeps_the_output_and_explains() {
        let llm = llm(vec![
            text(r#"{"answer": "2 + 3 = 5", "value": 5}"#),
            text(&layout("/nope")),
            text(&layout("/also_nope")),
        ])
        .await;
        let fx = fixture("reject", None, "all");
        with_display_prompt(&fx);
        let app = platform(&fx, false, &format!("{}/v1", llm.uri()))
            .await
            .router();

        let (s, v) = run(&app, json!({"input": REQUEST, "ui": "a2ui"})).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(v["output"], json!({"answer": "2 + 3 = 5", "value": 5}));
        assert!(v["ui"].is_null() && v.get("ui").is_some());
        let d = &v["trace"]["display"];
        assert_eq!(
            (d["status"].as_str(), d["attempts"].as_u64()),
            (Some("failed"), Some(2))
        );
        assert!(d["reason"].as_str().unwrap().contains("/also_nope"));
    }

    #[tokio::test]
    async fn missing_prompt_or_disabled_is_unavailable() {
        let llm = llm(vec![text(r#"{"answer": "5", "value": 5}"#)]).await;
        let fx = fixture("reject", None, "all");
        let app = platform(&fx, false, &format!("{}/v1", llm.uri()))
            .await
            .router();
        let (s, v) = run(&app, json!({"input": REQUEST, "ui": "a2ui"})).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(v["trace"]["display"]["status"], "unavailable");
        assert!(
            v["trace"]["display"]["reason"]
                .as_str()
                .unwrap()
                .contains("display_agent")
        );

        let llm = common::llm(vec![text(r#"{"answer": "5", "value": 5}"#)]).await;
        let fx = fixture("reject", None, "all");
        with_display_prompt(&fx);
        set_config(&fx, "routing:", "display: {enabled: false}\nrouting:");
        let app = platform(&fx, false, &format!("{}/v1", llm.uri()))
            .await
            .router();
        let (_, v) = run(&app, json!({"input": REQUEST, "ui": "a2ui"})).await;
        assert_eq!(v["trace"]["display"]["status"], "unavailable");
        assert!(
            v["trace"]["display"]["reason"]
                .as_str()
                .unwrap()
                .contains("disabled")
        );
    }
}
