use std::env;
use std::path::PathBuf;

use agentic_core::auth::check_secret;

#[derive(Debug, Clone)]
pub struct McpServerConfig {
    /// Policy namespace; the first segment of `<server>.<tool>`.
    pub name: String,
    pub listen: String,
    pub jwt_secret: String,
    pub issuer: String,
    /// `None` means default-deny for every tool call.
    pub policy_file: Option<PathBuf>,
}

impl McpServerConfig {
    /// Reads `MCP_SERVER_NAME`, `MCP_LISTEN`, `PLATFORM_JWT_SECRET`, `JWT_ISSUER`, `POLICY_FILE`.
    pub fn from_env(default_listen: &str) -> Result<Self, String> {
        Self::from_lookup(default_listen, |k| env::var(k).ok())
    }

    pub fn from_lookup(
        default_listen: &str,
        get: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, String> {
        let non_empty = |k: &str| get(k).filter(|v| !v.is_empty());
        let required = |k: &str| non_empty(k).ok_or_else(|| format!("{k} is required"));
        let jwt_secret = required("PLATFORM_JWT_SECRET")?;
        check_secret("PLATFORM_JWT_SECRET", &jwt_secret).map_err(|e| e.to_string())?;
        Ok(Self {
            name: required("MCP_SERVER_NAME")?,
            listen: non_empty("MCP_LISTEN").unwrap_or_else(|| default_listen.to_owned()),
            jwt_secret,
            issuer: non_empty("JWT_ISSUER").unwrap_or_else(|| "agentic-platform".to_owned()),
            policy_file: non_empty("POLICY_FILE").map(PathBuf::from),
        })
    }
}

#[cfg(test)]
mod tests {
    use agentic_core::auth::PLACEHOLDER_SECRET;

    use super::*;

    fn cfg(pairs: &'static [(&'static str, &'static str)]) -> Result<McpServerConfig, String> {
        McpServerConfig::from_lookup("0.0.0.0:3001", |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| (*v).to_owned())
        })
    }

    const SECRET: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn defaults() {
        let c = cfg(&[("MCP_SERVER_NAME", "docs"), ("PLATFORM_JWT_SECRET", SECRET)]).unwrap();
        assert_eq!(c.listen, "0.0.0.0:3001");
        assert_eq!(c.issuer, "agentic-platform");
        assert!(c.policy_file.is_none());
    }

    #[test]
    fn name_required() {
        assert!(cfg(&[("PLATFORM_JWT_SECRET", SECRET)]).is_err());
    }

    #[test]
    fn secret_required() {
        assert!(cfg(&[("MCP_SERVER_NAME", "docs")]).is_err());
    }

    #[test]
    fn short_secret_rejected() {
        let err = cfg(&[("MCP_SERVER_NAME", "docs"), ("PLATFORM_JWT_SECRET", "s")]).unwrap_err();
        assert!(err.contains("32 bytes"), "{err}");
    }

    #[test]
    fn placeholder_secret_rejected() {
        let err = cfg(&[
            ("MCP_SERVER_NAME", "docs"),
            ("PLATFORM_JWT_SECRET", PLACEHOLDER_SECRET),
        ])
        .unwrap_err();
        assert!(err.contains("placeholder"), "{err}");
    }
}
