use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use adk_agent::LlmAgentBuilder;
use adk_core::{AdkError, Content, Llm, RunConfig, StreamingMode, Tool, ToolContext};
use adk_model::{
    GeminiModel, OllamaConfig, OllamaModel, RetryConfig,
    anthropic::{AnthropicClient, AnthropicConfig},
    openai::OpenAIClient,
};
use adk_runner::Runner;
use adk_session::{CreateRequest, InMemorySessionService, SessionService};
use async_trait::async_trait;
use futures::StreamExt;
use serde_json::Value;

use super::{AgentEngine, Conversation, TokenUsage, ToolSpec, TurnError, TurnOutput, TurnRequest};
use crate::{
    PlatformError, Result,
    llm::{GATEWAY_PROVIDER, GatewaySettings, gateway_alias, missing_gateway_block},
};

const APP: &str = "agentic-platform";
const USER: &str = "user";
const AGENT: &str = "agent";
const DEFAULT_OLLAMA_HOST: &str = "http://localhost:11434";
const PROVIDERS: &str = "ollama, openai, anthropic, gemini, gateway";

type ModelKey = (String, Option<String>);

#[derive(Default)]
pub struct AdkEngine {
    models: Mutex<HashMap<ModelKey, Arc<dyn Llm>>>,
    gateway: Option<Arc<GatewaySettings>>,
}

impl AdkEngine {
    pub fn new() -> Self {
        Self::default()
    }

    /// Enables `gateway:<alias>`. The adk OpenAI-compatible client cannot carry extra body fields or
    /// headers, so no attribution metadata reaches the gateway; only the virtual key does.
    pub fn with_gateway(gateway: Option<Arc<GatewaySettings>>) -> Self {
        Self {
            gateway,
            ..Self::default()
        }
    }

    /// Registers a prebuilt model for `spec`, bypassing provider construction (tests, custom providers).
    pub fn insert_model(&self, spec: &str, model: Arc<dyn Llm>) {
        self.lock().insert((spec.to_owned(), None), model);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<ModelKey, Arc<dyn Llm>>> {
        self.models.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `team` selects the virtual key of a `gateway:` spec.
    fn model(&self, spec: &str, team: Option<&str>) -> Result<Arc<dyn Llm>> {
        if let Some(m) = self.lock().get(&(spec.to_owned(), None)) {
            return Ok(m.clone());
        }
        let cache_key = match gateway_alias(spec) {
            Some(_) => {
                let settings = self
                    .gateway
                    .as_ref()
                    .ok_or_else(|| missing_gateway_block(spec))?;
                (spec.to_owned(), Some(settings.key_for(team)?.to_owned()))
            }
            None => (spec.to_owned(), None),
        };
        if let Some(m) = self.lock().get(&cache_key) {
            return Ok(m.clone());
        }
        let m = build_model(
            spec,
            &|k| std::env::var(k).ok(),
            self.gateway.as_deref(),
            team,
        )?;
        Ok(self.lock().entry(cache_key).or_insert(m).clone())
    }
}

fn build_model(
    spec: &str,
    env: &dyn Fn(&str) -> Option<String>,
    gateway: Option<&GatewaySettings>,
    team: Option<&str>,
) -> Result<Arc<dyn Llm>> {
    let bad = |e: &dyn std::fmt::Display| PlatformError::Model(format!("`{spec}`: {e}"));
    let env = |k: &str| env(k).filter(|v| !v.is_empty());
    let (provider, model) = spec.split_once(':').ok_or_else(|| {
        bad(&format_args!(
            "expected `<provider>:<model>` with provider one of: {PROVIDERS}"
        ))
    })?;
    let key = |name: &str| env(name).ok_or_else(|| bad(&format_args!("{name} is not set")));
    Ok(match provider {
        GATEWAY_PROVIDER => {
            let settings = gateway.ok_or_else(|| missing_gateway_block(spec))?;
            let alias = gateway_alias(spec).ok_or_else(|| bad(&"empty gateway alias"))?;
            Arc::new(
                OpenAIClient::compatible(settings.key_for(team)?, settings.base_url(), alias)
                    .map_err(|e| bad(&e))?
                    // The guard owns retries; adk-model's default of 3 would multiply them.
                    .with_retry_config(RetryConfig::disabled()),
            )
        }
        "ollama" => {
            let host = env("OLLAMA_API_BASE_URL")
                .or_else(|| env("OLLAMA_HOST"))
                .unwrap_or_else(|| DEFAULT_OLLAMA_HOST.into());
            Arc::new(OllamaModel::new(OllamaConfig::with_host(host, model)).map_err(|e| bad(&e))?)
        }
        "openai" => match env("OPENAI_BASE_URL") {
            // Compatible servers (vLLM, Ollama, mocks) usually ignore the key but the client requires one.
            Some(base) => Arc::new(
                OpenAIClient::compatible(
                    env("OPENAI_API_KEY").unwrap_or_else(|| "unused".into()),
                    base,
                    model,
                )
                .map_err(|e| bad(&e))?,
            ),
            None => Arc::new(
                OpenAIClient::compatible(
                    key("OPENAI_API_KEY")?,
                    "https://api.openai.com/v1",
                    model,
                )
                .map_err(|e| bad(&e))?,
            ),
        },
        "anthropic" => {
            let mut cfg = AnthropicConfig::new(key("ANTHROPIC_API_KEY")?, model);
            if let Some(base) = env("ANTHROPIC_BASE_URL") {
                cfg = cfg.with_base_url(base);
            }
            Arc::new(AnthropicClient::new(cfg).map_err(|e| bad(&e))?)
        }
        "gemini" => Arc::new(GeminiModel::from_env(model).map_err(|e| bad(&e))?),
        other => {
            return Err(bad(&format_args!(
                "unsupported provider `{other}` for the adk engine; supported: {PROVIDERS}"
            )));
        }
    })
}

/// Adapts an engine-neutral `ToolSpec`; `Err` reaches the model as a tool error rather than failing the run.
struct DynTool(ToolSpec);

impl DynTool {
    async fn invoke(&self, args: Value) -> adk_core::Result<Value> {
        (self.0.call)(args)
            .await
            .map(Value::String)
            .map_err(AdkError::tool)
    }
}

#[async_trait]
impl Tool for DynTool {
    fn name(&self) -> &str {
        &self.0.name
    }

    fn description(&self) -> &str {
        &self.0.description
    }

    fn parameters_schema(&self) -> Option<Value> {
        Some(self.0.parameters.clone())
    }

    async fn execute(&self, _ctx: Arc<dyn ToolContext>, args: Value) -> adk_core::Result<Value> {
        self.invoke(args).await
    }
}

/// `LlmAgent` signals an exhausted iteration budget only through an agent error message.
fn is_max_iterations(e: &AdkError) -> bool {
    let m = e.to_string();
    e.is_agent() && m.contains("Max iterations (") && m.contains(") exceeded")
}

fn classify(e: &AdkError, usage: TokenUsage) -> TurnError {
    let message = e.to_string();
    if is_max_iterations(e) {
        TurnError::MaxTurns { message, usage }
    } else {
        TurnError::Infra { message, usage }
    }
}

fn infra(message: impl Into<String>, usage: TokenUsage) -> TurnError {
    TurnError::Infra {
        message: message.into(),
        usage,
    }
}

struct AdkConversation {
    sessions: Arc<InMemorySessionService>,
    session_id: Option<String>,
}

async fn create_session(sessions: &InMemorySessionService) -> adk_core::Result<String> {
    let s = sessions
        .create(CreateRequest {
            app_name: APP.into(),
            user_id: USER.into(),
            session_id: None,
            state: HashMap::new(),
        })
        .await?;
    Ok(s.id().to_owned())
}

fn team_of(req: &TurnRequest) -> Option<&str> {
    Some(req.attribution.team.as_str()).filter(|t| !t.is_empty())
}

fn clamp(n: i32) -> u64 {
    u64::try_from(n).unwrap_or(0)
}

async fn run(
    model: Arc<dyn Llm>,
    req: &TurnRequest,
    tools: &[ToolSpec],
    max_iterations: usize,
    sessions: Arc<InMemorySessionService>,
    session_id: &str,
    message: String,
) -> std::result::Result<TurnOutput, TurnError> {
    let mut usage = TokenUsage::default();
    let mut builder = LlmAgentBuilder::new(AGENT)
        .instruction(req.preamble.clone())
        .model(model)
        .max_iterations(u32::try_from(max_iterations).unwrap_or(u32::MAX));
    if let Some(t) = req.temperature {
        builder = builder.temperature(t as f32);
    }
    for t in tools {
        builder = builder.tool(Arc::new(DynTool(t.clone())));
    }
    let agent = builder.build().map_err(|e| classify(&e, usage))?;
    let runner = Runner::builder()
        .app_name(APP)
        .agent(Arc::new(agent))
        .session_service(sessions)
        .run_config(RunConfig {
            streaming_mode: StreamingMode::None,
            ..Default::default()
        })
        .build()
        .map_err(|e| classify(&e, usage))?;
    let mut stream = runner
        .run_str(USER, session_id, Content::new("user").with_text(message))
        .await
        .map_err(|e| classify(&e, usage))?;

    let mut text = String::new();
    while let Some(ev) = stream.next().await {
        let ev = ev.map_err(|e| classify(&e, usage))?;
        let r = &ev.llm_response;
        if r.error_code.is_some() || r.error_message.is_some() {
            return Err(infra(
                format!(
                    "model error {}: {}",
                    r.error_code.as_deref().unwrap_or("unknown"),
                    r.error_message.as_deref().unwrap_or("")
                ),
                usage,
            ));
        }
        if r.partial {
            continue;
        }
        if let Some(m) = &r.usage_metadata {
            usage.input_tokens += clamp(m.prompt_token_count);
            usage.output_tokens += clamp(m.candidates_token_count);
        }
        if ev.author != "user"
            && let Some(c) = &r.content
            && !c.has_function_calls()
        {
            let t: String = c.parts.iter().filter_map(|p| p.text()).collect();
            if !t.is_empty() {
                text = t;
            }
        }
    }
    if text.is_empty() {
        return Err(infra("empty response", usage));
    }
    Ok(TurnOutput { text, usage })
}

#[async_trait]
impl AgentEngine for AdkEngine {
    fn name(&self) -> &'static str {
        "adk"
    }

    fn check_model(&self, spec: &str) -> Result<()> {
        self.model(spec, None).map(drop)
    }

    fn conversation(&self) -> Conversation {
        Conversation::new(AdkConversation {
            sessions: Arc::new(InMemorySessionService::new()),
            session_id: None,
        })
    }

    async fn chat(
        &self,
        req: &TurnRequest,
        conv: &mut Conversation,
        message: String,
    ) -> std::result::Result<TurnOutput, TurnError> {
        let none = TokenUsage::default();
        let state = conv
            .state_mut::<AdkConversation>()
            .ok_or_else(|| infra("conversation was not created by the adk engine", none))?;
        let model = self
            .model(&req.model_spec, team_of(req))
            .map_err(|e| infra(e.to_string(), none))?;
        let session_id = match &state.session_id {
            Some(id) => id.clone(),
            None => {
                let id = create_session(&state.sessions)
                    .await
                    .map_err(|e| classify(&e, none))?;
                state.session_id = Some(id.clone());
                id
            }
        };
        run(
            model,
            req,
            &req.tools,
            req.max_turns,
            state.sessions.clone(),
            &session_id,
            message,
        )
        .await
    }

    async fn prompt(
        &self,
        req: &TurnRequest,
        message: String,
    ) -> std::result::Result<TurnOutput, TurnError> {
        let none = TokenUsage::default();
        let model = self
            .model(&req.model_spec, team_of(req))
            .map_err(|e| infra(e.to_string(), none))?;
        let sessions = Arc::new(InMemorySessionService::new());
        let session_id = create_session(&sessions)
            .await
            .map_err(|e| classify(&e, none))?;
        run(model, req, &[], 1, sessions, &session_id, message).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use adk_core::{LlmRequest, LlmResponse, LlmResponseStream, UsageMetadata};
    use serde_json::json;

    use super::*;

    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| (*v).to_owned())
        }
    }

    fn err_text(spec: &str, env: &[(&str, &str)]) -> String {
        match build_model(spec, &env_of(env), None, None) {
            Ok(_) => panic!("{spec} should not build"),
            Err(e) => e.to_string(),
        }
    }

    #[test]
    fn spec_errors_name_the_supported_providers() {
        assert!(err_text("nocolon", &[]).contains(PROVIDERS));
        let e = err_text("mistral:large", &[]);
        assert!(e.contains("mistral") && e.contains(PROVIDERS), "{e}");
    }

    #[test]
    fn keys_are_required_without_a_compatible_base() {
        assert!(err_text("openai:m", &[]).contains("OPENAI_API_KEY"));
        assert!(err_text("anthropic:m", &[]).contains("ANTHROPIC_API_KEY"));
        assert!(err_text("openai:m", &[("OPENAI_API_KEY", "")]).contains("OPENAI_API_KEY"));
    }

    #[test]
    fn providers_build_from_env() {
        for (spec, env) in [
            ("ollama:gpt-oss", vec![]),
            ("ollama:gpt-oss", vec![("OLLAMA_HOST", "http://h:1")]),
            ("openai:m", vec![("OPENAI_API_KEY", "k")]),
            (
                "openai:m",
                vec![("OPENAI_BASE_URL", "http://localhost:1/v1")],
            ),
            ("anthropic:m", vec![("ANTHROPIC_API_KEY", "k")]),
            (
                "anthropic:m",
                vec![
                    ("ANTHROPIC_API_KEY", "k"),
                    ("ANTHROPIC_BASE_URL", "https://x.example"),
                ],
            ),
        ] {
            build_model(spec, &env_of(&env), None, None).unwrap_or_else(|e| panic!("{spec}: {e}"));
        }
    }

    fn gateway_settings(env: &[(&str, &str)]) -> Arc<GatewaySettings> {
        let cfg =
            serde_yaml::from_str("{url: \"http://g:4000\", team_keys: {docs-team: TEAM_KEY}}")
                .unwrap();
        Arc::new(GatewaySettings::resolve(&cfg, &env_of(env)))
    }

    #[test]
    fn gateway_spec_needs_a_block_and_a_key() {
        let e = err_text("gateway:x", &[]);
        assert!(
            e.contains("model spec `gateway:x` needs an `llm_gateway` block in platform.yaml"),
            "{e}"
        );
        assert!(AdkEngine::new().check_model("gateway:x").is_err());

        let no_key = AdkEngine::with_gateway(Some(gateway_settings(&[])));
        let e = no_key.check_model("gateway:x").unwrap_err().to_string();
        assert!(e.contains("LLM_GATEWAY_API_KEY"), "{e}");
    }

    #[test]
    fn gateway_models_are_cached_per_key() {
        let engine = AdkEngine::with_gateway(Some(gateway_settings(&[
            ("LLM_GATEWAY_API_KEY", "d"),
            ("TEAM_KEY", "t"),
        ])));
        for team in [Some("docs-team"), Some("other"), None] {
            engine.model("gateway:a", team).unwrap();
        }
        assert_eq!(engine.lock().len(), 2);
    }

    #[test]
    fn max_iterations_is_classified_and_the_rest_is_infra() {
        let u = TokenUsage {
            input_tokens: 3,
            output_tokens: 4,
        };
        let e = classify(&AdkError::agent("Max iterations (5) exceeded"), u);
        assert!(matches!(&e, TurnError::MaxTurns { usage, .. } if *usage == u));
        for other in [
            AdkError::agent("agent exploded"),
            AdkError::model("Max iterations (5) exceeded"),
            AdkError::tool("boom"),
        ] {
            assert!(matches!(classify(&other, u), TurnError::Infra { usage, .. } if usage == u));
        }
    }

    fn spec(
        f: impl Fn(Value) -> std::result::Result<String, String> + Send + Sync + 'static,
    ) -> ToolSpec {
        let f = Arc::new(f);
        ToolSpec {
            name: "t".into(),
            description: "d".into(),
            parameters: json!({"type": "object"}),
            call: Arc::new(move |v| {
                let f = f.clone();
                Box::pin(async move { f(v) })
            }),
        }
    }

    #[tokio::test]
    async fn dyn_tool_maps_results_and_errors() {
        let ok = DynTool(spec(|v| Ok(v["x"].to_string())));
        assert_eq!(ok.invoke(json!({"x": 5})).await.unwrap(), json!("5"));
        let err = DynTool(spec(|_| Err("denied".into())))
            .invoke(json!({}))
            .await
            .unwrap_err();
        assert!(err.is_tool() && err.to_string().contains("denied"), "{err}");
        assert_eq!(ok.parameters_schema(), Some(json!({"type": "object"})));
    }

    /// Replays scripted responses and records the contents of every request.
    struct Script {
        replies: Mutex<Vec<LlmResponse>>,
        seen: Mutex<Vec<String>>,
    }

    impl Script {
        fn new(replies: Vec<LlmResponse>) -> Arc<Self> {
            let mut replies = replies;
            replies.reverse();
            Arc::new(Self {
                replies: Mutex::new(replies),
                seen: Mutex::default(),
            })
        }
    }

    #[async_trait]
    impl Llm for Script {
        fn name(&self) -> &str {
            "script"
        }

        async fn generate_content(
            &self,
            req: LlmRequest,
            _stream: bool,
        ) -> adk_core::Result<LlmResponseStream> {
            self.seen
                .lock()
                .unwrap()
                .push(serde_json::to_string(&req.contents).unwrap());
            let r = self
                .replies
                .lock()
                .unwrap()
                .pop()
                .ok_or_else(|| AdkError::model("script exhausted"))?;
            Ok(Box::pin(futures::stream::iter([Ok(r)])))
        }
    }

    fn reply(text: &str, input: i32, output: i32) -> LlmResponse {
        let mut r = LlmResponse::new(Content::new("model").with_text(text));
        r.usage_metadata = Some(UsageMetadata {
            prompt_token_count: input,
            candidates_token_count: output,
            total_token_count: input + output,
            ..Default::default()
        });
        r.turn_complete = true;
        r
    }

    fn turn_req(model: &str) -> TurnRequest {
        TurnRequest {
            model_spec: model.into(),
            preamble: "be brief".into(),
            temperature: Some(0.0),
            max_turns: 4,
            tools: vec![],
            caller: None,
            request_id: String::new(),
            attribution: Default::default(),
        }
    }

    #[tokio::test]
    async fn history_survives_a_new_agent_and_runner_per_chat() {
        let llm = Script::new(vec![
            reply("first answer", 10, 2),
            reply("second answer", 20, 3),
        ]);
        let engine = AdkEngine::new();
        engine.insert_model("t:m", llm.clone());
        let mut conv = engine.conversation();
        let req = turn_req("t:m");

        let a = engine.chat(&req, &mut conv, "q1".into()).await.unwrap();
        assert_eq!(a.text, "first answer");
        assert_eq!((a.usage.input_tokens, a.usage.output_tokens), (10, 2));
        let b = engine.chat(&req, &mut conv, "q2".into()).await.unwrap();
        assert_eq!(b.text, "second answer");

        let second_request = llm.seen.lock().unwrap()[1].clone();
        assert!(
            second_request.contains("q1") && second_request.contains("first answer"),
            "{second_request}"
        );
    }

    #[tokio::test]
    async fn prompt_retains_nothing() {
        let llm = Script::new(vec![reply("a", 1, 1), reply("b", 1, 1)]);
        let engine = AdkEngine::new();
        engine.insert_model("t:m", llm.clone());
        engine.prompt(&turn_req("t:m"), "q1".into()).await.unwrap();
        engine.prompt(&turn_req("t:m"), "q2".into()).await.unwrap();
        assert!(!llm.seen.lock().unwrap()[1].contains("q1"));
    }

    #[tokio::test]
    async fn provider_error_event_and_empty_text_are_infra() {
        let failing = LlmResponse {
            error_code: Some("429".into()),
            error_message: Some("slow down".into()),
            ..Default::default()
        };
        let engine = AdkEngine::new();
        engine.insert_model("t:f", Script::new(vec![failing]));
        let e = engine
            .prompt(&turn_req("t:f"), "q".into())
            .await
            .unwrap_err();
        assert!(matches!(&e, TurnError::Infra { message, .. } if message.contains("slow down")));

        engine.insert_model("t:e", Script::new(vec![LlmResponse::default()]));
        let e = engine
            .prompt(&turn_req("t:e"), "q".into())
            .await
            .unwrap_err();
        assert!(matches!(&e, TurnError::Infra { message, .. } if message == "empty response"));
    }
}
