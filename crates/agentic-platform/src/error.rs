#[derive(Debug, thiserror::Error)]
pub enum PlatformError {
    #[error("config: {0}")]
    Config(String),
    #[error("{0}")]
    Core(#[from] agentic_core::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("yaml: {0}")]
    Yaml(#[from] serde_yaml::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("model: {0}")]
    Model(String),
    #[error("mcp: {0}")]
    Mcp(#[from] crate::mcp::McpError),
}

pub type Result<T> = std::result::Result<T, PlatformError>;
