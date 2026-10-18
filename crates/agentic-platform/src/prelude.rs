//! Derive macros expand to `serde::` / `schemars::` paths, so projects using them need those crates as direct dependencies.
pub use crate::{
    AgentOutput, Evidence, Platform, PlatformBuilder, Specialist, ToolCallRecord, Violation,
    config_path_from_env_or_args,
};
pub use schemars::JsonSchema;
pub use serde::{Deserialize, Serialize};
