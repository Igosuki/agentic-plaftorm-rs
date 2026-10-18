mod common;

#[cfg(not(feature = "adk"))]
mod without_adk {
    use super::common;
    use agentic_platform::prelude::*;
    use common::{CalcOut, fixture, set_config};

    #[tokio::test]
    async fn adk_engine_without_the_feature_fails_the_build() {
        let fx = fixture("reject", None, "all");
        set_config(&fx, "app: test-app\n", "app: test-app\nengine: adk\n");
        let b = Platform::from_config(&fx.config)
            .unwrap()
            .jwt_keys(common::keys())
            .specialist(Specialist::<CalcOut>::new("calc_agent"));
        let err = b.build().await.err().expect("build must fail").to_string();
        assert!(
            err.contains(
                "engine `adk` requires building agentic-platform with the `adk` cargo feature"
            ),
            "{err}"
        );
    }
}

#[cfg(feature = "adk")]
mod with_adk {
    use super::common;
    use agentic_platform::prelude::*;
    use axum::http::StatusCode;
    use common::{CalcOut, bearer, fixture, llm, mcp, send, set_config, token};
    use serde_json::{Value, json};

    /// adk-model requests `stream: true` whatever the run's streaming mode, and reads usage only from the
    /// chunk carrying `finish_reason` (OpenAI proper sends it in a later chunk without choices).
    fn sse(delta: Value, finish: &str) -> wiremock::ResponseTemplate {
        let chunk = |choices: Value, usage: Value| {
            let mut c = json!({"id": "cmpl-1", "object": "chat.completion.chunk", "created": 0, "model": "mock-model", "choices": choices});
            if !usage.is_null() {
                c["usage"] = usage;
            }
            format!("data: {c}\n\n")
        };
        let usage = json!({"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15});
        let body = chunk(
            json!([{"index": 0, "delta": delta, "finish_reason": null}]),
            Value::Null,
        ) + &chunk(
            json!([{"index": 0, "delta": {}, "finish_reason": finish}]),
            usage,
        ) + "data: [DONE]\n\n";
        wiremock::ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
    }

    fn text(content: &str) -> wiremock::ResponseTemplate {
        sse(json!({"role": "assistant", "content": content}), "stop")
    }

    fn tool_call(name: &str, args: &str) -> wiremock::ResponseTemplate {
        sse(
            json!({"role": "assistant", "tool_calls": [
                {"index": 0, "id": "call_1", "type": "function", "function": {"name": name, "arguments": args}}
            ]}),
            "tool_calls",
        )
    }

    #[tokio::test]
    async fn adk_engine_retries_with_history_and_one_tool_call() {
        use adk_model::openai::OpenAIClient;
        use agentic_platform::engine::AdkEngine;
        use std::sync::Arc;

        let llm = llm(vec![
            tool_call("add", r#"{"a":2,"b":3}"#),
            text("I think the answer is five"),
            text("Sure: {\"answer\": \"2 + 3 = 5\", \"value\": 5}"),
        ])
        .await;
        let mcp = mcp(json!({"jsonrpc": "2.0", "id": 1, "result": {"content": [{"type": "text", "text": "5"}], "resultType": "complete"}})).await;
        let fx = fixture("reject", Some(&format!("{}/mcp", mcp.uri())), "all");
        set_config(&fx, "app: test-app\n", "app: test-app\nengine: adk\n");
        set_config(&fx, "mock:model", "openai:mock-model");

        let engine = AdkEngine::new();
        engine.insert_model(
            "openai:mock-model",
            Arc::new(
                OpenAIClient::compatible("test-key", format!("{}/v1", llm.uri()), "mock-model")
                    .unwrap(),
            ),
        );
        let app = Platform::from_config(&fx.config)
            .unwrap()
            .jwt_keys(common::keys())
            .specialist(
                Specialist::<CalcOut>::new("calc_agent")
                    .description("Does arithmetic")
                    .mcp_server("calc"),
            )
            .engine("adk", Arc::new(engine))
            .build()
            .await
            .unwrap()
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
        assert_eq!(v["output"], json!({"answer": "2 + 3 = 5", "value": 5}));
        assert_eq!(v["trace"]["engine"], "adk");
        let outcomes: Vec<&str> = v["trace"]["attempts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["outcome"].as_str().unwrap())
            .collect();
        assert_eq!(outcomes, ["schema_violation", "ok"]);
        assert_eq!(v["trace"]["tool_calls"].as_array().unwrap().len(), 1);
        assert_eq!(v["trace"]["tool_calls"][0]["outcome"], "ok");
        assert!(v["trace"]["usage"]["input_tokens"].as_u64().unwrap() > 0);
        assert!(v["trace"]["usage"]["output_tokens"].as_u64().unwrap() > 0);

        let llm_requests = llm.received_requests().await.unwrap();
        assert_eq!(llm_requests.len(), 3);
        let last: Value = serde_json::from_slice(&llm_requests[2].body).unwrap();
        let transcript = last["messages"].to_string();
        assert!(
            transcript.contains("I think the answer is five"),
            "retry must carry the prior answer: {transcript}"
        );
        assert!(transcript.contains("You are calc v2."));
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
        assert_eq!(claims.sub, "alice");
    }

    #[tokio::test]
    async fn adk_gateway_spec_uses_team_key_and_sends_no_metadata() {
        unsafe {
            std::env::set_var("GW_ADK_DEFAULT", "default-key");
            std::env::set_var("GW_ADK_DOCS", "docs-team-key");
        }
        let gw = llm(vec![
            text(r#"{"answer": "five", "value": 5}"#),
            text(r#"{"answer": "five", "value": 5}"#),
        ])
        .await;
        let fx = fixture("reject", None, "all");
        set_config(&fx, "app: test-app\n", "app: test-app\nengine: adk\n");
        set_config(
            &fx,
            "models: {default: \"mock:model\"}",
            &format!(
                "models: {{default: \"gateway:test-alias\"}}\nllm_gateway: {{url: \"{}\", api_key_env: GW_ADK_DEFAULT, team_keys: {{docs-team: GW_ADK_DOCS}}}}",
                gw.uri()
            ),
        );
        let app = Platform::from_config(&fx.config)
            .unwrap()
            .jwt_keys(common::keys())
            .specialist(Specialist::<CalcOut>::new("calc_agent").description("d"))
            .build()
            .await
            .unwrap()
            .router();
        for key in ["alice-key", "root-key"] {
            let t = token(&app, key).await;
            let (s, v) = send(
                &app,
                "POST",
                "/v1/agents/run",
                Some(&t),
                Some(json!({"input": "add sum"})),
            )
            .await;
            assert_eq!(s, StatusCode::OK, "{v}");
        }
        let reqs = gw.received_requests().await.unwrap();
        assert_eq!(reqs[0].url.path(), "/v1/chat/completions");
        assert_eq!(bearer(&reqs[0]), "docs-team-key");
        assert_eq!(bearer(&reqs[1]), "default-key");
        let b: Value = serde_json::from_slice(&reqs[0].body).unwrap();
        assert_eq!(b["model"], "test-alias");
        assert!(b.get("metadata").is_none(), "{b}");
    }
}
