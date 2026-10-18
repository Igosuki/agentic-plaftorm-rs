use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rig::{
    DynModel,
    agent::{AgentBuilder, AgentHook, HookContext, OutcomeAction, OutcomeEvent},
    completion::PromptError,
    message::Message,
    operation::Completion,
    tool::{DynamicTool, ToolExecutionError, ToolOutput},
};

use super::{AgentEngine, Conversation, TokenUsage, ToolSpec, TurnError, TurnOutput, TurnRequest};
use crate::{
    Result,
    llm::{GatewaySettings, ModelCache},
};

/// Sums the usage of every completion in a run, including runs that end in an error.
#[derive(Clone, Default)]
struct UsageMeter(Arc<Mutex<TokenUsage>>);

impl UsageMeter {
    fn get(&self) -> TokenUsage {
        *self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl AgentHook for UsageMeter {
    async fn on_outcome(&self, _ctx: &HookContext, event: OutcomeEvent<'_>) -> OutcomeAction {
        if let Some(c) = event.completion() {
            let mut u = self.0.lock().unwrap_or_else(|e| e.into_inner());
            u.input_tokens += c.usage.input_tokens.unwrap_or(0);
            u.output_tokens += c.usage.output_tokens.unwrap_or(0);
        }
        OutcomeAction::Proceed
    }
}

#[derive(Default)]
pub struct RigEngine {
    models: ModelCache,
}

impl RigEngine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_gateway(&self, settings: Option<Arc<GatewaySettings>>) {
        self.models.set_gateway(settings);
    }

    fn model_for(&self, req: &TurnRequest) -> Result<DynModel<Completion>> {
        let team = Some(req.attribution.team.as_str()).filter(|t| !t.is_empty());
        self.models.get(&req.model_spec, team)
    }

    fn gateway_params(&self, req: &TurnRequest) -> Option<serde_json::Value> {
        self.models
            .gateway()?
            .attribution_params(&req.model_spec, &req.attribution)
    }

    /// Registers a prebuilt model for `spec`, bypassing `model_from_spec` (tests, custom providers).
    pub fn insert_model(&self, spec: &str, model: DynModel<Completion>) {
        self.models.insert(spec, model);
    }
}

fn to_dynamic_tool(tool: ToolSpec) -> DynamicTool {
    let call = tool.call;
    DynamicTool::new(tool.name, tool.description, tool.parameters, move |args| {
        let call = call.clone();
        Box::pin(async move {
            call(args)
                .await
                .map(ToolOutput::text)
                .map_err(ToolExecutionError::other)
        })
    })
}

fn infra(message: String) -> TurnError {
    TurnError::Infra {
        message,
        usage: TokenUsage::default(),
    }
}

fn map_error(e: PromptError, usage: TokenUsage) -> TurnError {
    let message = e.to_string();
    match e {
        PromptError::MaxTurnsError { .. } => TurnError::MaxTurns { message, usage },
        _ => TurnError::Infra { message, usage },
    }
}

#[async_trait]
impl AgentEngine for RigEngine {
    fn name(&self) -> &'static str {
        "rig"
    }

    fn check_model(&self, spec: &str) -> Result<()> {
        self.models.get(spec, None).map(drop)
    }

    fn conversation(&self) -> Conversation {
        Conversation::new(Vec::<Message>::new())
    }

    async fn chat(
        &self,
        req: &TurnRequest,
        conv: &mut Conversation,
        message: String,
    ) -> std::result::Result<TurnOutput, TurnError> {
        let history = conv
            .state_mut::<Vec<Message>>()
            .ok_or_else(|| infra("conversation was not created by the rig engine".into()))?;
        let model = self.model_for(req).map_err(|e| infra(e.to_string()))?;
        let meter = UsageMeter::default();
        let mut builder = AgentBuilder::new(model)
            .preamble(req.preamble.clone())
            .default_max_turns(req.max_turns)
            .add_hook(meter.clone());
        if let Some(t) = req.temperature {
            builder = builder.temperature(t);
        }
        if let Some(params) = self.gateway_params(req) {
            builder = builder.additional_params(params);
        }
        let agent = if req.tools.is_empty() {
            builder.build()
        } else {
            builder
                .dynamic_tools(
                    req.tools
                        .iter()
                        .cloned()
                        .map(to_dynamic_tool)
                        .collect::<Vec<_>>(),
                )
                .build()
        };
        match agent.chat(message, history).await {
            Ok(resp) => Ok(TurnOutput {
                text: resp.output,
                usage: meter.get(),
            }),
            Err(e) => Err(map_error(e, meter.get())),
        }
    }

    async fn prompt(
        &self,
        req: &TurnRequest,
        message: String,
    ) -> std::result::Result<TurnOutput, TurnError> {
        let model = self.model_for(req).map_err(|e| infra(e.to_string()))?;
        let meter = UsageMeter::default();
        let mut builder = AgentBuilder::new(model)
            .preamble(req.preamble.clone())
            .add_hook(meter.clone());
        if let Some(t) = req.temperature {
            builder = builder.temperature(t);
        }
        if let Some(params) = self.gateway_params(req) {
            builder = builder.additional_params(params);
        }
        match builder.build().prompt(message).await {
            Ok(resp) => Ok(TurnOutput {
                text: resp.output,
                usage: meter.get(),
            }),
            Err(e) => Err(map_error(e, meter.get())),
        }
    }
}
