#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("jwt: {0}")]
    Jwt(#[from] jsonwebtoken::errors::Error),
    #[error("environment variable `{0}` is not set")]
    MissingEnv(String),
    #[error("`{var}` {reason}; generate one with `openssl rand -hex 32`")]
    WeakSecret { var: String, reason: &'static str },
    #[error("unterminated `${{` in config value")]
    UnterminatedExpansion,
    #[error("yaml: {0}")]
    Yaml(#[from] serde_yaml::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
