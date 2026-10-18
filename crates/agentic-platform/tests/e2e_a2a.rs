mod common;

#[cfg(not(feature = "a2a"))]
mod without_a2a {
    use super::common;
    use agentic_platform::prelude::*;
    use common::{CalcOut, fixture, set_config};

    async fn build_err(from: &str, to: &str) -> String {
        let fx = fixture("reject", None, "all");
        set_config(&fx, from, to);
        Platform::from_config(&fx.config)
            .unwrap()
            .jwt_keys(common::keys())
            .specialist(Specialist::<CalcOut>::new("calc_agent"))
            .build()
            .await
            .err()
            .expect("build must fail")
            .to_string()
    }

    #[tokio::test]
    async fn a2a_engine_without_the_feature_fails_the_build() {
        let err = build_err("app: test-app\n", "app: test-app\nengine: a2a\n").await;
        assert!(
            err.contains(
                "engine `a2a` requires building agentic-platform with the `a2a` cargo feature"
            ),
            "{err}"
        );
    }

    #[tokio::test]
    async fn a2a_server_without_the_feature_fails_the_build() {
        let err = build_err(
            "app: test-app\n",
            "app: test-app\na2a: {server: {enabled: true}}\n",
        )
        .await;
        assert!(err.contains("`a2a` cargo feature"), "{err}");
    }
}

#[cfg(feature = "a2a")]
mod with_a2a {
    use std::sync::{Arc, Mutex};

    use a2a_rs::{
        JsonRpcClient, Transport,
        adapter::{
            InMemoryTaskStorage, JsonRpcAdapter, SimpleAgentInfo,
            business::{Responder, ResponderMessageHandler},
            jsonrpc_router,
            streaming::InMemoryStreamingHandler,
        },
        domain::{A2AError, Message, Part, Role, SendCompletion, Task, TaskState, part},
    };
    use agentic_core::auth::{AUD_A2A, Claims};
    use agentic_platform::{
        engine::{A2aEngine, AgentEngine, TurnError, TurnRequest},
        prelude::*,
    };
    use async_trait::async_trait;
    use axum::{
        Router,
        body::Body,
        http::{Request, StatusCode, header::AUTHORIZATION},
        middleware::{self, Next},
        response::Response,
    };
    use buffa_types::google::protobuf::Struct;
    use common::{fixture, keys, llm, set_config, text};
    use serde_json::{Value, json};

    use super::common;

    const ISS: &str = "agentic-platform";

    async fn serve(router: Router) -> String {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, router).await.unwrap() });
        format!("http://{addr}")
    }

    fn user_token(aud_a2a: bool) -> String {
        let k = keys();
        if aud_a2a {
            let parent = claims();
            k.issue_delegated_token_for(&parent, AUD_A2A, "req-1", ISS, 60)
                .unwrap()
        } else {
            k.issue_user_token("alice", "docs-team", &["reader".to_owned()], ISS, 60)
                .unwrap()
        }
    }

    fn claims() -> Claims {
        let t = keys()
            .issue_user_token("alice", "docs-team", &["reader".to_owned()], ISS, 60)
            .unwrap();
        keys().verify(&t, "gateway", ISS).unwrap()
    }

    fn user_message(text: &str, meta: Value) -> Message {
        let mut b = Message::builder()
            .role(Role::ROLE_USER)
            .parts(vec![Part::text(text.into())])
            .message_id(uuid::Uuid::new_v4().to_string());
        if !meta.is_null() {
            let s: Struct = serde_json::from_value(meta).unwrap();
            b = b.metadata(s);
        }
        b.build()
    }

    async fn ask(
        url: &str,
        token: Option<&str>,
        text: &str,
        meta: Value,
    ) -> Result<Task, A2AError> {
        let client = match token {
            Some(t) => JsonRpcClient::try_with_auth(url.to_owned(), t.to_owned()),
            None => JsonRpcClient::try_new(url.to_owned()),
        }
        .unwrap();
        client
            .send_task_message(
                None,
                &user_message(text, meta),
                None,
                None,
                SendCompletion::WhenSettled,
            )
            .await
    }

    fn state(t: &Task) -> TaskState {
        t.status.as_option().unwrap().state.as_known().unwrap()
    }

    fn reply_parts(t: &Task) -> Vec<Part> {
        t.status
            .as_option()
            .and_then(|s| s.message.as_option())
            .map(|m| m.parts.clone())
            .unwrap_or_default()
    }

    fn reply_text(t: &Task) -> String {
        reply_parts(t)
            .iter()
            .filter_map(Part::get_text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn reply_data(t: &Task) -> Value {
        reply_parts(&t.clone())
            .iter()
            .find_map(|p| match &p.content {
                Some(part::Content::Data(d)) => serde_json::to_value(&**d).ok(),
                _ => None,
            })
            .expect("reply has a data part")
    }

    fn a2a_fixture(extra: &str) -> common::Fixture {
        let fx = fixture("reject", None, "all");
        set_config(
            &fx,
            "app: test-app\n",
            &format!("app: test-app\na2a: {{server: {{enabled: true}}}}\n{extra}"),
        );
        fx
    }

    async fn server_platform(fx: &common::Fixture, llm_url: &str) -> Platform {
        common::platform(fx, false, llm_url).await
    }

    #[tokio::test]
    async fn server_answers_with_output_data_part_and_trace() {
        let llm = llm(vec![text(r#"{"answer": "5", "value": 5}"#)]).await;
        let fx = a2a_fixture("");
        let base = serve(
            server_platform(&fx, &format!("{}/v1", llm.uri()))
                .await
                .router(),
        )
        .await;
        let url = format!("{base}/a2a");

        let card: Value = reqwest::get(format!("{base}/.well-known/agent-card.json"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(card["name"], "test-app");
        let skills: Vec<&str> = card["skills"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["id"].as_str().unwrap())
            .collect();
        assert!(
            skills.contains(&"calc_agent") && skills.contains(&"auto"),
            "{card}"
        );

        let task = ask(
            &url,
            Some(&user_token(false)),
            "please add 2 and 3",
            Value::Null,
        )
        .await
        .unwrap();
        assert_eq!(state(&task), TaskState::TASK_STATE_COMPLETED);
        assert_eq!(
            serde_json::from_str::<Value>(&reply_text(&task)).unwrap(),
            json!({"answer": "5", "value": 5})
        );
        let data = reply_data(&task);
        assert_eq!(data["agent"], "calc_agent");
        assert_eq!(data["output"]["answer"], "5");
        assert_eq!(data["trace"]["usage"]["input_tokens"], 10.0);
        assert!(data["trace"]["latency_ms"].is_number());
        let mut keys: Vec<&str> = data
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, ["agent", "output", "prompt", "request_id", "trace"]);
        assert_eq!(
            data["trace"].as_object().unwrap().len(),
            2,
            "attempt and tool-call detail stay out of the stored task"
        );
        assert_eq!(card["capabilities"]["pushNotifications"], false);
    }

    #[tokio::test]
    async fn explicit_skill_hint_skips_the_intent_gate_and_auto_routes() {
        let llm = llm(vec![
            text(r#"{"answer": "x", "value": 1}"#),
            text(r#"{"answer": "y", "value": 2}"#),
        ])
        .await;
        let fx = a2a_fixture("");
        let base = serve(
            server_platform(&fx, &format!("{}/v1", llm.uri()))
                .await
                .router(),
        )
        .await;
        let url = format!("{base}/a2a");
        let tok = user_token(true);

        let t = ask(&url, Some(&tok), "hello", json!({"skill": "calc_agent"}))
            .await
            .unwrap();
        assert_eq!(state(&t), TaskState::TASK_STATE_COMPLETED);
        assert_eq!(reply_data(&t)["agent"], "calc_agent");

        let t = ask(&url, Some(&tok), "add numbers", json!({"skill": "auto"}))
            .await
            .unwrap();
        assert_eq!(state(&t), TaskState::TASK_STATE_COMPLETED);
        assert_eq!(reply_data(&t)["agent"], "calc_agent");

        let t = ask(&url, Some(&tok), "hello", json!({"skill": "nope"}))
            .await
            .unwrap();
        assert_eq!(state(&t), TaskState::TASK_STATE_REJECTED);
        assert!(
            reply_text(&t).starts_with("invalid_request:"),
            "{}",
            reply_text(&t)
        );
    }

    #[tokio::test]
    async fn unauthenticated_calls_get_http_401() {
        let llm = llm(vec![]).await;
        let fx = a2a_fixture("");
        let base = serve(
            server_platform(&fx, &format!("{}/v1", llm.uri()))
                .await
                .router(),
        )
        .await;
        let url = format!("{base}/a2a");

        assert!(ask(&url, None, "add 1 and 2", Value::Null).await.is_err());
        let http = reqwest::Client::new();
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": "SendMessage", "params": {}});
        for bearer in [None, Some("garbage".to_owned()), Some(mcp_token())] {
            let mut rb = http.post(&url).json(&body);
            if let Some(b) = bearer {
                rb = rb.bearer_auth(b);
            }
            assert_eq!(rb.send().await.unwrap().status(), 401);
        }
        let card = http
            .get(format!("{base}/.well-known/agent-card.json"))
            .send()
            .await
            .unwrap();
        assert_eq!(card.status(), 200);
    }

    fn mcp_token() -> String {
        keys()
            .issue_delegated_token(&claims(), "r", ISS, 60)
            .unwrap()
    }

    #[tokio::test]
    async fn pipeline_errors_become_rejected_and_failed_tasks() {
        let llm = llm(vec![text("nope"), text("nope"), text("nope"), text("nope")]).await;
        let fx = a2a_fixture("");
        let base = serve(
            server_platform(&fx, &format!("{}/v1", llm.uri()))
                .await
                .router(),
        )
        .await;
        let url = format!("{base}/a2a");
        let tok = user_token(false);

        let t = ask(&url, Some(&tok), "tell me a story", Value::Null)
            .await
            .unwrap();
        assert_eq!(state(&t), TaskState::TASK_STATE_REJECTED);
        assert!(reply_text(&t).starts_with("unclassified:"));
        let d = reply_data(&t);
        assert_eq!(d["error"]["code"], "unclassified");
        assert!(d["request_id"].is_string());

        let t = ask(&url, Some(&tok), "add 2 and 3", Value::Null)
            .await
            .unwrap();
        assert_eq!(state(&t), TaskState::TASK_STATE_FAILED);
        assert!(reply_text(&t).starts_with("agent_failed:"));
        assert!(reply_data(&t)["trace"]["usage"].is_object());
        assert!(reply_data(&t)["trace"].get("attempts").is_none());
    }

    #[derive(Debug, Serialize, Deserialize, JsonSchema)]
    struct Strict {
        answer: String,
        value: i64,
        unit: String,
    }

    impl AgentOutput for Strict {}

    /// Accepts what `Strict` rejects, serialising a missing unit as `null`.
    #[derive(Debug, Serialize, Deserialize, JsonSchema)]
    struct Lenient {
        answer: String,
        value: i64,
        unit: Option<String>,
    }

    impl AgentOutput for Lenient {}

    /// Records the `Authorization` header of every request that reaches the wrapped router.
    fn observe(router: Router, seen: Arc<Mutex<Vec<String>>>) -> Router {
        router.layer(middleware::from_fn(
            move |req: Request<Body>, next: Next| {
                let seen = seen.clone();
                async move {
                    if let Some(a) = req
                        .headers()
                        .get(AUTHORIZATION)
                        .and_then(|v| v.to_str().ok())
                    {
                        seen.lock()
                            .unwrap()
                            .push(a.trim_start_matches("Bearer ").to_owned());
                    }
                    next.run(req).await
                }
            },
        ))
    }

    #[tokio::test]
    async fn platform_calls_platform_across_a2a() {
        // B is the remote platform; its LLM answers the schema-violating-for-A reply first, then a valid one.
        let b_llm = llm(vec![
            text(r#"{"answer": "5", "value": 5}"#),
            text(r#"{"answer": "5", "value": 5, "unit": "apples"}"#),
        ])
        .await;
        let b_fx = a2a_fixture("");
        let b = Platform::from_config(&b_fx.config)
            .unwrap()
            .jwt_keys(keys())
            .model(
                "mock:model",
                common::mock_model(&format!("{}/v1", b_llm.uri())),
            )
            .specialist(Specialist::<Lenient>::new("calc_agent").description("Calculator"))
            .build()
            .await
            .unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let b_base = serve(observe(b.router(), seen.clone())).await;

        let a_fx = fixture("reject", None, "all");
        set_config(
            &a_fx,
            "app: test-app\n",
            &format!(
                "app: test-app\nremote_agents:\n  b: {{url: \"{b_base}/a2a\", skill: calc_agent}}\n"
            ),
        );
        std::fs::write(
            a_fx.dir.path().join("prompts/remote_calc.yaml"),
            "id: remote_calc\nlive: 1.0.0\nversions:\n  - {version: 1.0.0, model: \"a2a:b\", template: \"Ask the remote calculator.\"}\n",
        )
        .unwrap();
        std::fs::write(
            a_fx.dir.path().join("intents.yaml"),
            "min_score: 2\nmin_margin: 1\nagents:\n  remote_calc:\n    keywords: {add: 2}\n",
        )
        .unwrap();
        let a = Platform::from_config(&a_fx.config)
            .unwrap()
            .jwt_keys(keys())
            .model("mock:model", common::mock_model("http://127.0.0.1:1/v1"))
            .specialist(
                Specialist::<Strict>::new("remote_calc")
                    .engine("a2a")
                    .description("Calculator behind A2A"),
            )
            .build()
            .await
            .unwrap();
        let app = a.router();
        let tok = common::token(&app, "alice-key").await;
        let (status, body) = common::send(
            &app,
            "POST",
            "/v1/agents/run",
            Some(&tok),
            Some(json!({"input": "add 2 and 3"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            body["output"],
            json!({"answer": "5", "value": 5, "unit": "apples"})
        );
        assert_eq!(body["trace"]["engine"], "a2a");
        let attempts = body["trace"]["attempts"].as_array().unwrap();
        assert_eq!(attempts[0]["outcome"], "schema_violation");
        assert_eq!(attempts[1]["outcome"], "ok");
        // Two remote runs of one 10/5-token completion each, reported in B's trace.
        assert_eq!(body["trace"]["usage"]["input_tokens"], 20);
        assert_eq!(body["trace"]["usage"]["output_tokens"], 10);

        let bodies: Vec<String> = b_llm
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| String::from_utf8_lossy(&r.body).into_owned())
            .collect();
        assert_eq!(bodies.len(), 2);
        assert!(
            bodies[0].contains("Respond ONLY with a JSON object"),
            "A's schema instructions must reach B"
        );
        assert!(bodies[0].contains("unit"));
        assert!(bodies[1].contains("Conversation so far"), "{}", bodies[1]);
        assert!(bodies[1].contains(r#"assistant: {"#), "{}", bodies[1]);
        assert!(bodies[1].contains(r#"\"unit\":null"#), "{}", bodies[1]);
        assert!(bodies[1].contains("was rejected"));

        let tokens = seen.lock().unwrap().clone();
        assert!(!tokens.is_empty());
        for t in &tokens {
            let c = keys().verify(t, AUD_A2A, ISS).unwrap();
            assert_eq!((c.sub.as_str(), c.team.as_str()), ("alice", "docs-team"));
            assert_eq!(c.roles, ["reader"]);
            assert!(c.request_id.is_some());
        }
    }

    struct Canned {
        reply: String,
        state: TaskState,
        seen_text: Arc<Mutex<Vec<String>>>,
        seen_ids: Arc<Mutex<Vec<(String, String)>>>,
    }

    #[async_trait]
    impl Responder for Canned {
        async fn respond(
            &self,
            m: &Message,
            task: &Task,
        ) -> Result<(Message, TaskState), A2AError> {
            let conversation = m
                .metadata
                .as_option()
                .and_then(|s| serde_json::to_value(s).ok())
                .and_then(|v| v["conversation_id"].as_str().map(str::to_owned))
                .unwrap_or_default();
            self.seen_ids
                .lock()
                .unwrap()
                .push((m.context_id.clone(), conversation));
            self.seen_text.lock().unwrap().push(
                m.parts
                    .iter()
                    .filter_map(Part::get_text)
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
            let reply = Message::builder()
                .role(Role::ROLE_AGENT)
                .parts(vec![Part::text(self.reply.clone())])
                .message_id(uuid::Uuid::new_v4().to_string())
                .task_id(task.id.clone())
                .context_id(m.context_id.clone())
                .build();
            Ok((reply, self.state))
        }
    }

    async fn plain_remote(
        reply: &str,
        state: TaskState,
        bearer: Option<&str>,
    ) -> (String, Arc<Mutex<Vec<String>>>) {
        let (url, text, _) = plain_remote_ids(reply, state, bearer).await;
        (url, text)
    }

    type SeenIds = Arc<Mutex<Vec<(String, String)>>>;

    async fn plain_remote_ids(
        reply: &str,
        state: TaskState,
        bearer: Option<&str>,
    ) -> (String, Arc<Mutex<Vec<String>>>, SeenIds) {
        let seen_text = Arc::new(Mutex::new(Vec::new()));
        let seen_ids: SeenIds = Arc::new(Mutex::new(Vec::new()));
        let storage = InMemoryTaskStorage::new();
        let streaming = InMemoryStreamingHandler::new();
        let handler = ResponderMessageHandler::new(
            storage.clone(),
            streaming.clone(),
            storage.push_notifier(),
            Canned {
                reply: reply.into(),
                state,
                seen_text: seen_text.clone(),
                seen_ids: seen_ids.clone(),
            },
        );
        let info = SimpleAgentInfo::new("plain".into(), "http://localhost".into());
        let adapter = Arc::new(
            JsonRpcAdapter::new(handler, storage.clone(), storage, info)
                .with_streaming_handler(streaming),
        );
        let mut router = jsonrpc_router(adapter);
        if let Some(expected) = bearer.map(str::to_owned) {
            router = router.layer(middleware::from_fn(
                move |req: Request<Body>, next: Next| {
                    let expected = expected.clone();
                    async move {
                        let ok = req
                            .headers()
                            .get(AUTHORIZATION)
                            .and_then(|v| v.to_str().ok())
                            .is_some_and(|v| v == format!("Bearer {expected}"));
                        if ok {
                            next.run(req).await
                        } else {
                            Response::builder()
                                .status(401)
                                .body(Body::from("unauthorized"))
                                .unwrap()
                        }
                    }
                },
            ));
        }
        (serve(router).await, seen_text, seen_ids)
    }

    fn engine(url: &str, auth: &str) -> A2aEngine {
        let remotes = serde_yaml::from_str(&format!(
            "r: {{url: \"{url}\", auth: {auth}, timeout_secs: 5, skill: s1}}"
        ))
        .unwrap();
        A2aEngine::new("test-app", remotes, keys(), ISS, 60)
    }

    fn turn() -> TurnRequest {
        TurnRequest {
            model_spec: "a2a:r".into(),
            preamble: "PRE".into(),
            temperature: None,
            max_turns: 1,
            tools: Vec::new(),
            caller: Some(claims()),
            request_id: "req-9".into(),
            attribution: Default::default(),
        }
    }

    #[tokio::test]
    async fn engine_extracts_text_from_a_plain_a2a_server() {
        let (url, seen) =
            plain_remote("hello from remote", TaskState::TASK_STATE_COMPLETED, None).await;
        let e = engine(&url, "none");
        let mut conv = e.conversation();
        let out = e.chat(&turn(), &mut conv, "first".into()).await.unwrap();
        assert_eq!(out.text, "hello from remote");
        assert_eq!(out.usage.total(), 0);
        let out = e.chat(&turn(), &mut conv, "second".into()).await.unwrap();
        assert_eq!(out.text, "hello from remote");
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen[0], "PRE\n\nfirst");
        assert!(seen[1].contains("user: first\nassistant: hello from remote"));
        assert!(seen[1].ends_with("user: second"));
        let one_shot = e.prompt(&turn(), "solo".into()).await.unwrap();
        assert_eq!(one_shot.text, "hello from remote");
    }

    #[tokio::test]
    async fn engine_maps_failed_state_to_infra_with_the_remote_message() {
        let (url, _) =
            plain_remote("agent_failed: kaboom", TaskState::TASK_STATE_FAILED, None).await;
        let e = engine(&url, "none");
        let err = e
            .chat(&turn(), &mut e.conversation(), "q".into())
            .await
            .unwrap_err();
        let TurnError::Infra { message, .. } = err else {
            panic!("expected infra")
        };
        assert!(
            message.contains("kaboom") && message.contains("`r`"),
            "{message}"
        );
    }

    #[tokio::test]
    async fn engine_reports_a_clear_error_on_401_and_sends_delegated_tokens() {
        let (url, _) = plain_remote(
            "ok",
            TaskState::TASK_STATE_COMPLETED,
            Some("the-right-token"),
        )
        .await;
        let e = engine(&url, "delegated");
        let err = e
            .chat(&turn(), &mut e.conversation(), "q".into())
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "remote `r` rejected credentials (401)");
        let err = engine(&url, "none")
            .chat(&turn(), &mut e.conversation(), "q".into())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("401"), "{err}");

        let mut no_caller = turn();
        no_caller.caller = None;
        let err = e
            .chat(&no_caller, &mut e.conversation(), "q".into())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no caller"), "{err}");
    }

    #[tokio::test]
    async fn every_call_gets_a_fresh_context_id_and_a_stable_conversation_id() {
        let (url, _, ids) = plain_remote_ids("ok", TaskState::TASK_STATE_COMPLETED, None).await;
        let e = engine(&url, "none");
        let mut conv = e.conversation();
        e.chat(&turn(), &mut conv, "one".into()).await.unwrap();
        e.chat(&turn(), &mut conv, "two".into()).await.unwrap();
        e.prompt(&turn(), "solo".into()).await.unwrap();
        let ids = ids.lock().unwrap().clone();
        assert_eq!(ids.len(), 3);
        assert_ne!(ids[0].0, ids[1].0, "context ids must not repeat");
        assert_ne!(ids[1].0, ids[2].0);
        assert_eq!(ids[0].1, ids[1].1, "one conversation correlates its calls");
        assert_ne!(ids[1].1, ids[2].1);
        assert!(ids.iter().all(|(c, v)| !c.is_empty() && !v.is_empty()));
    }

    #[tokio::test]
    async fn engine_maps_rejected_and_credential_failures_to_rejected() {
        let (url, _) = plain_remote("not allowed", TaskState::TASK_STATE_REJECTED, None).await;
        let e = engine(&url, "none");
        let err = e
            .chat(&turn(), &mut e.conversation(), "q".into())
            .await
            .unwrap_err();
        assert!(
            matches!(&err, TurnError::Rejected { message, .. } if message.contains("not allowed")),
            "{err}"
        );

        let (url, _) = plain_remote("ok", TaskState::TASK_STATE_COMPLETED, Some("tok")).await;
        let err = engine(&url, "none")
            .chat(&turn(), &mut e.conversation(), "q".into())
            .await
            .unwrap_err();
        assert!(matches!(err, TurnError::Rejected { .. }), "{err}");
    }

    #[tokio::test]
    async fn rejection_stops_the_guard_after_one_attempt() {
        let (url, seen) = plain_remote("not allowed", TaskState::TASK_STATE_REJECTED, None).await;
        let a_fx = fixture("reject", None, "all");
        set_config(
            &a_fx,
            "app: test-app\n",
            &format!("app: test-app\nremote_agents:\n  b: {{url: \"{url}\", auth: none}}\n"),
        );
        std::fs::write(
            a_fx.dir.path().join("prompts/remote_calc.yaml"),
            "id: remote_calc\nlive: 1.0.0\nversions:\n  - {version: 1.0.0, model: \"a2a:b\", template: \"Ask.\"}\n",
        )
        .unwrap();
        std::fs::write(
            a_fx.dir.path().join("intents.yaml"),
            "min_score: 2\nmin_margin: 1\nagents:\n  remote_calc:\n    keywords: {add: 2}\n",
        )
        .unwrap();
        let a = Platform::from_config(&a_fx.config)
            .unwrap()
            .jwt_keys(keys())
            .model("mock:model", common::mock_model("http://127.0.0.1:1/v1"))
            .specialist(Specialist::<Lenient>::new("remote_calc").engine("a2a"))
            .build()
            .await
            .unwrap();
        let app = a.router();
        let tok = common::token(&app, "alice-key").await;
        let (status, body) = common::send(
            &app,
            "POST",
            "/v1/agents/run",
            Some(&tok),
            Some(json!({"input": "add 2 and 3"})),
        )
        .await;
        assert!(status.is_server_error(), "{status} {body}");
        let attempts = body["trace"]["attempts"]
            .as_array()
            .expect("trace attempts");
        assert_eq!(attempts.len(), 1, "{body}");
        assert_eq!(attempts[0]["outcome"], "rejected");
        assert_eq!(seen.lock().unwrap().len(), 1, "the remote is called once");
    }

    #[tokio::test]
    async fn delegated_tokens_are_peer_bound_and_downscoped() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (url, _) = plain_remote("ok", TaskState::TASK_STATE_COMPLETED, None).await;
        let router = {
            let storage = InMemoryTaskStorage::new();
            let streaming = InMemoryStreamingHandler::new();
            let handler = ResponderMessageHandler::new(
                storage.clone(),
                streaming.clone(),
                storage.push_notifier(),
                Canned {
                    reply: "ok".into(),
                    state: TaskState::TASK_STATE_COMPLETED,
                    seen_text: Arc::default(),
                    seen_ids: Arc::default(),
                },
            );
            let info = SimpleAgentInfo::new("p".into(), "http://localhost".into());
            let adapter = Arc::new(
                JsonRpcAdapter::new(handler, storage.clone(), storage, info)
                    .with_streaming_handler(streaming),
            );
            observe(jsonrpc_router(adapter), seen.clone())
        };
        let url2 = serve(router).await;
        drop(url);

        let remotes = serde_yaml::from_str(&format!(
            "r: {{url: \"{url2}\", audience: \"a2a:ops\", timeout_secs: 5}}\nlimited: {{url: \"{url2}\", audience: \"a2a:ops\", timeout_secs: 5, delegate_roles: [reader]}}"
        ))
        .unwrap();
        let e = A2aEngine::new("test-app", remotes, keys(), ISS, 60);
        let mut caller = claims();
        caller.roles = vec![
            "reader".into(),
            "admin".into(),
            "platform".into(),
            "engineer".into(),
        ];
        let mut req = turn();
        req.caller = Some(caller.clone());
        e.chat(&req, &mut e.conversation(), "q".into())
            .await
            .unwrap();
        req.model_spec = "a2a:limited".into();
        e.chat(&req, &mut e.conversation(), "q".into())
            .await
            .unwrap();

        let tokens = seen.lock().unwrap().clone();
        assert_eq!(tokens.len(), 2);
        let default = keys().verify(&tokens[0], "a2a:ops", ISS).unwrap();
        assert_eq!(default.roles, ["reader", "engineer"]);
        let limited = keys().verify(&tokens[1], "a2a:ops", ISS).unwrap();
        assert_eq!(limited.roles, ["reader"]);
        for t in &tokens {
            assert!(keys().verify(t, AUD_A2A, ISS).is_err());
            assert!(keys().verify(t, "gateway", ISS).is_err());
        }
    }

    #[tokio::test]
    async fn engine_treats_an_unreachable_remote_as_infra() {
        let e = engine("http://127.0.0.1:9", "none");
        let err = e
            .chat(&turn(), &mut e.conversation(), "q".into())
            .await
            .unwrap_err();
        assert!(matches!(err, TurnError::Infra { .. }));
        assert!(err.to_string().contains("unreachable"), "{err}");
    }

    fn token_for(sub: &str, team: &str, aud: &str) -> String {
        let mut c = claims();
        c.sub = sub.into();
        c.team = team.into();
        keys()
            .issue_delegated_token_for(&c, aud, "r", ISS, 60)
            .unwrap()
    }

    async fn rpc(url: &str, token: &str, method: &str, params: Value) -> Value {
        reqwest::Client::new()
            .post(url)
            .bearer_auth(token)
            .json(&json!({"jsonrpc": "2.0", "id": 7, "method": method, "params": params}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    fn code(v: &Value) -> i64 {
        v["error"]["code"]
            .as_i64()
            .unwrap_or_else(|| panic!("no error in {v}"))
    }

    #[tokio::test]
    async fn tasks_are_private_to_their_creator_and_unsafe_methods_are_gone() {
        const METHOD_NOT_FOUND: i64 = -32601;
        const TASK_NOT_FOUND: i64 = -32001;
        let llm = llm(vec![text(r#"{"answer": "5", "value": 5}"#)]).await;
        let fx = a2a_fixture("");
        let base = serve(
            server_platform(&fx, &format!("{}/v1", llm.uri()))
                .await
                .router(),
        )
        .await;
        let url = format!("{base}/a2a");
        let alice = token_for("alice", "docs-team", "gateway");
        let bob = token_for("bob", "other-team", "gateway");

        let task = ask(&url, Some(&alice), "add 2 and 3", Value::Null)
            .await
            .unwrap();
        assert_eq!(state(&task), TaskState::TASK_STATE_COMPLETED);
        let (id, ctx) = (task.id.clone(), task.context_id.clone());

        let own = rpc(&url, &alice, "GetTask", json!({"id": id})).await;
        assert_eq!(own["result"]["id"], id.as_str(), "{own}");

        for method in ["GetTask", "CancelTask", "SubscribeToTask"] {
            let r = rpc(&url, &bob, method, json!({"id": id})).await;
            assert_eq!(code(&r), TASK_NOT_FOUND, "{method}: {r}");
            let unknown = rpc(&url, &bob, method, json!({"id": "no-such-task"})).await;
            assert_eq!(
                r["error"]["message"].as_str().map(|m| m.replace(&id, "X")),
                unknown["error"]["message"]
                    .as_str()
                    .map(|m| m.replace("no-such-task", "X")),
                "existence must not leak"
            );
        }
        let still = rpc(&url, &alice, "GetTask", json!({"id": id})).await;
        assert_eq!(
            still["result"]["status"]["state"], "TASK_STATE_COMPLETED",
            "bob's CancelTask must not touch it: {still}"
        );

        for params in [
            json!({"message": {"messageId": "m1", "role": "ROLE_USER", "taskId": id,
                    "parts": [{"text": "hijack"}]}}),
            json!({"message": {"messageId": "m2", "role": "ROLE_USER", "task_id": id,
                    "parts": [{"text": "hijack"}]}}),
            json!({"message": {"messageId": "m3", "role": "ROLE_USER", "contextId": ctx,
                    "parts": [{"text": "hijack"}]}}),
        ] {
            for method in ["SendMessage", "SendStreamingMessage"] {
                let r = rpc(&url, &bob, method, params.clone()).await;
                assert_eq!(code(&r), TASK_NOT_FOUND, "{method}: {r}");
            }
        }
        let history = rpc(&url, &alice, "GetTask", json!({"id": id})).await;
        assert!(
            !history.to_string().contains("hijack"),
            "bob's message must not reach alice's task: {history}"
        );

        for method in [
            "ListTasks",
            "CreateTaskPushNotificationConfig",
            "GetTaskPushNotificationConfig",
            "ListTaskPushNotificationConfigs",
            "DeleteTaskPushNotificationConfig",
            "GetExtendedAgentCard",
            "tasks/list",
        ] {
            for who in [&alice, &bob] {
                let r = rpc(&url, who, method, json!({"taskId": id, "id": id})).await;
                assert_eq!(code(&r), METHOD_NOT_FOUND, "{method}: {r}");
            }
        }
        let with_push = rpc(
            &url,
            &alice,
            "SendMessage",
            json!({"message": {"messageId": "m4", "role": "ROLE_USER", "parts": [{"text": "add 1 and 1"}]},
                   "configuration": {"taskPushNotificationConfig": {"url": "http://127.0.0.1:1/hook"}}}),
        )
        .await;
        assert_eq!(code(&with_push), -32003, "{with_push}");

        let rest = reqwest::Client::new()
            .get(format!("{url}/tasks"))
            .bearer_auth(&alice)
            .send()
            .await
            .unwrap();
        assert_eq!(rest.status(), 404, "the REST binding is not served");
    }

    #[tokio::test]
    async fn peer_bound_audiences_are_accepted_and_others_rejected() {
        let llm = llm(vec![]).await;
        let fx = a2a_fixture("");
        let base = serve(
            server_platform(&fx, &format!("{}/v1", llm.uri()))
                .await
                .router(),
        )
        .await;
        let url = format!("{base}/a2a");
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": "GetTask", "params": {"id": "t"}});
        let status = |aud: &str| {
            let t = token_for("alice", "docs-team", aud);
            let url = url.clone();
            let body = body.clone();
            async move {
                reqwest::Client::new()
                    .post(url)
                    .bearer_auth(t)
                    .json(&body)
                    .send()
                    .await
                    .unwrap()
                    .status()
            }
        };
        for aud in ["a2a", "a2a:test-app", "gateway"] {
            assert_eq!(status(aud).await, 200, "{aud}");
        }
        for aud in ["a2a:other", "mcp"] {
            assert_eq!(status(aud).await, 401, "{aud}");
        }
    }

    #[tokio::test]
    async fn unknown_remote_fails_the_model_check() {
        let e = engine("http://x", "none");
        assert!(e.check_model("a2a:r").is_ok());
        assert!(e.check_model("a2a:other").is_err());
    }
}
