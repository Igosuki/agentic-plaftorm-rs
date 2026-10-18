//! Reusable fastmcp-rust server scaffolding: JWT auth, policy enforcement, env config, runtime.

mod auth;
mod config;
mod policy;
mod run;

pub use auth::JwtAuthProvider;
pub use config::McpServerConfig;
pub use policy::{PolicyMiddleware, decide};
pub use run::run;
