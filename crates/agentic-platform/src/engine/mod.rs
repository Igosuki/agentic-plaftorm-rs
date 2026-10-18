#[cfg(feature = "a2a")]
pub mod a2a;
#[cfg(feature = "adk")]
pub mod adk;
pub mod rig;

use std::{any::Any, sync::Arc};

use agentic_core::auth::Claims;
use async_trait::async_trait;
use serde::Serialize;
use serde_json::Value;

use crate::{Result, specialist::BoxFuture};

#[cfg(feature = "a2a")]
pub use self::a2a::A2aEngine;
#[cfg(feature = "adk")]
pub use self::adk::AdkEngine;
pub use self::rig::RigEngine;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl TokenUsage {
    pub fn total(self) -> u64 {
        self.input_tokens + self.output_tokens
    }
}

/// An engine-neutral tool. `call` returns the text shown to the model; `Err` is a tool failure the model may see.
/// Evidence recording and delegated-token minting happen inside `call`, never in the engine.
#[derive(Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema of the arguments object.
    pub parameters: Value,
    pub call:
        Arc<dyn Fn(Value) -> BoxFuture<'static, std::result::Result<String, String>> + Send + Sync>,
}

/// Who and what a turn runs for; engines forward it only to backends that record it (AI gateways).
#[derive(Debug, Clone, Default)]
pub struct Attribution {
    pub user: String,
    pub team: String,
    pub application: String,
    pub use_case: String,
    pub agent: String,
    pub prompt_version: String,
    pub request_id: String,
}

pub struct TurnRequest {
    /// `<provider>:<model>`; the engine resolves it.
    pub model_spec: String,
    pub preamble: String,
    pub temperature: Option<f64>,
    /// Model-call budget per `chat`, tool round-trips included. Ignored by `prompt`.
    pub max_turns: usize,
    pub tools: Vec<ToolSpec>,
    /// Identity of the end user the turn runs for; engines that call other services on their behalf use it.
    pub caller: Option<Claims>,
    /// Correlation id of the originating request; empty outside a request.
    pub request_id: String,
    /// Selects the gateway virtual key (by `team`) and fills the gateway metadata.
    pub attribution: Attribution,
}

#[derive(Debug)]
pub struct TurnOutput {
    pub text: String,
    pub usage: TokenUsage,
}

/// `usage` covers every completion made before the failure.
#[derive(Debug, thiserror::Error)]
pub enum TurnError {
    #[error("{message}")]
    MaxTurns { message: String, usage: TokenUsage },
    #[error("{message}")]
    Infra { message: String, usage: TokenUsage },
    /// The backend refused the request in a way a retry cannot change (authorization, invalid input).
    #[error("{message}")]
    Rejected { message: String, usage: TokenUsage },
}

impl TurnError {
    pub fn usage(&self) -> TokenUsage {
        match self {
            Self::MaxTurns { usage, .. }
            | Self::Infra { usage, .. }
            | Self::Rejected { usage, .. } => *usage,
        }
    }
}

/// Engine-owned history carried across `chat` calls; only the engine that created it can read it.
pub struct Conversation(Box<dyn Any + Send>);

impl Conversation {
    pub fn new<T: Send + 'static>(state: T) -> Self {
        Self(Box::new(state))
    }

    pub fn state_mut<T: 'static>(&mut self) -> Option<&mut T> {
        self.0.downcast_mut()
    }
}

#[async_trait]
pub trait AgentEngine: Send + Sync {
    /// Registry key and the value reported in `trace.engine`.
    fn name(&self) -> &'static str;

    /// Fails when `spec` cannot be resolved to a model; called before a request runs.
    fn check_model(&self, spec: &str) -> Result<()>;

    fn conversation(&self) -> Conversation;

    /// One agent loop (tools included) over `conv`; the exchange is appended to `conv` on success.
    async fn chat(
        &self,
        req: &TurnRequest,
        conv: &mut Conversation,
        message: String,
    ) -> std::result::Result<TurnOutput, TurnError>;

    /// A single tool-less completion with no retained history.
    async fn prompt(
        &self,
        req: &TurnRequest,
        message: String,
    ) -> std::result::Result<TurnOutput, TurnError>;
}
