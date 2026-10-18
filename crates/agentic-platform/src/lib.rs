#[cfg(feature = "a2a")]
pub mod a2a_server;
pub mod config;
pub mod coordinator;
pub mod engine;
pub mod error;
pub mod gateway;
pub mod guard;
pub mod intent;
pub mod llm;
pub mod mcp;
pub mod pipeline;
pub mod platform;
pub mod prelude;
pub mod prompts;
pub mod specialist;
pub mod telemetry;

pub use error::{PlatformError, Result};
pub use guard::{AgentOutput, Violation};
pub use mcp::{Evidence, ToolCallRecord};
pub use platform::{Platform, PlatformBuilder, config_path_from_env_or_args};
pub use specialist::Specialist;
