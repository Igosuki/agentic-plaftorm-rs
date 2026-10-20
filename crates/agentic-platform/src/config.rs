use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use agentic_core::{
    auth::{AUD_A2A, AUD_GATEWAY, AUD_MCP},
    config::expand_env,
    policy::Policy,
};
use serde::Deserialize;

use crate::{PlatformError, Result, intent::IntentConfig};

#[derive(Debug, Clone, Deserialize)]
pub struct AuthConfig {
    #[serde(default = "default_secret_env")]
    pub jwt_secret_env: String,
    #[serde(default = "default_issuer")]
    pub issuer: String,
    #[serde(default = "default_token_ttl")]
    pub token_ttl_secs: i64,
    #[serde(default = "default_delegated_ttl")]
    pub delegated_ttl_secs: i64,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            jwt_secret_env: default_secret_env(),
            issuer: default_issuer(),
            token_ttl_secs: default_token_ttl(),
            delegated_ttl_secs: default_delegated_ttl(),
        }
    }
}

fn default_secret_env() -> String {
    "PLATFORM_JWT_SECRET".into()
}
fn default_issuer() -> String {
    "agentic-platform".into()
}
fn default_token_ttl() -> i64 {
    3600
}
fn default_delegated_ttl() -> i64 {
    60
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelsConfig {
    pub default: String,
    #[serde(default)]
    pub coordinator: Option<String>,
}

impl ModelsConfig {
    pub fn coordinator(&self) -> &str {
        self.coordinator.as_deref().unwrap_or(&self.default)
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq)]
pub struct Price {
    pub input: f64,
    pub output: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GuardConfig {
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
    #[serde(default = "default_max_total_tokens")]
    pub max_total_tokens: u64,
    #[serde(default = "default_backoff_base_ms")]
    pub backoff_base_ms: u64,
}

impl Default for GuardConfig {
    fn default() -> Self {
        Self {
            max_attempts: default_max_attempts(),
            max_total_tokens: default_max_total_tokens(),
            backoff_base_ms: default_backoff_base_ms(),
        }
    }
}

fn default_max_attempts() -> u32 {
    3
}
fn default_max_total_tokens() -> u64 {
    60_000
}
fn default_backoff_base_ms() -> u64 {
    250
}

/// The opt-in display agent that lays out a validated output as an A2UI surface.
#[derive(Debug, Clone, Deserialize)]
pub struct DisplayConfig {
    #[serde(default = "default_true", deserialize_with = "lenient_bool")]
    pub enabled: bool,
    /// Defaults to `models.default`.
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default = "default_display_attempts")]
    pub max_attempts: u32,
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            model: None,
            max_attempts: default_display_attempts(),
        }
    }
}

fn default_display_attempts() -> u32 {
    2
}

#[derive(Debug, Clone, Deserialize)]
pub struct RateLimitConfig {
    #[serde(default = "default_rpm")]
    pub requests_per_minute_per_team: u32,
    /// `POST /auth/token` attempts per client IP per minute; 0 disables.
    #[serde(default = "default_auth_rpm")]
    pub auth_rate_limit_per_minute: u32,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            requests_per_minute_per_team: default_rpm(),
            auth_rate_limit_per_minute: default_auth_rpm(),
        }
    }
}

fn default_rpm() -> u32 {
    60
}
fn default_auth_rpm() -> u32 {
    10
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum UnclassifiedMode {
    #[default]
    Reject,
    Llm,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RoutingConfig {
    #[serde(default)]
    pub unclassified: UnclassifiedMode,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ToolVisibility {
    Filtered,
    #[default]
    All,
}

#[derive(Debug, Clone, Deserialize)]
pub struct McpServerConfig {
    pub url: String,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RemoteAuth {
    /// Mint a delegated token carrying the caller's identity.
    #[default]
    Delegated,
    None,
}

/// A remote A2A agent addressable as the model spec `a2a:<name>`.
#[derive(Debug, Clone, Deserialize)]
pub struct RemoteAgentConfig {
    pub url: String,
    #[serde(default = "default_remote_audience")]
    pub audience: String,
    #[serde(default)]
    pub auth: RemoteAuth,
    #[serde(default = "default_remote_timeout")]
    pub timeout_secs: u64,
    /// Sent as message metadata `skill`; the remote decides what it selects.
    #[serde(default)]
    pub skill: Option<String>,
    /// Roles the delegated token may carry (intersected with the caller's); unset drops `admin` and `platform`.
    #[serde(default)]
    pub delegate_roles: Option<Vec<String>>,
}

fn default_remote_audience() -> String {
    "a2a".into()
}
fn default_remote_timeout() -> u64 {
    120
}

#[derive(Debug, Clone, Deserialize)]
pub struct A2aServerConfig {
    #[serde(default, deserialize_with = "lenient_bool")]
    pub enabled: bool,
    #[serde(default = "default_a2a_path")]
    pub path: String,
    /// URL advertised in the agent card; defaults to the listen address plus `path`.
    #[serde(default)]
    pub public_url: Option<String>,
    /// Token audiences the A2A endpoint accepts; unset means `gateway`, `a2a` and `a2a:<app>`.
    #[serde(default)]
    pub accept_audiences: Option<Vec<String>>,
    /// Idle time after which a settled task is dropped from the in-memory store.
    #[serde(default = "default_task_ttl_secs")]
    pub task_ttl_secs: u64,
}

impl A2aServerConfig {
    pub fn audiences(&self, app: &str) -> Vec<String> {
        self.accept_audiences.clone().unwrap_or_else(|| {
            vec![
                AUD_GATEWAY.into(),
                AUD_A2A.into(),
                format!("{AUD_A2A}:{app}"),
            ]
        })
    }
}

impl Default for A2aServerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            path: default_a2a_path(),
            public_url: None,
            accept_audiences: None,
            task_ttl_secs: default_task_ttl_secs(),
        }
    }
}

fn default_a2a_path() -> String {
    "/a2a".into()
}
fn default_task_ttl_secs() -> u64 {
    900
}

/// Accepts YAML booleans and the strings `true`/`false`, which env interpolation into a quoted scalar produces.
fn lenient_bool<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<bool, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum B {
        Bool(bool),
        Str(String),
    }
    match B::deserialize(d)? {
        B::Bool(b) => Ok(b),
        B::Str(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" => Ok(true),
            "false" | "0" | "no" | "" => Ok(false),
            other => Err(serde::de::Error::custom(format!(
                "expected a boolean, got `{other}`"
            ))),
        },
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct A2aConfig {
    #[serde(default)]
    pub server: A2aServerConfig,
}

/// OpenAI-compatible AI gateway (LiteLLM Proxy, Portkey, Kong, Envoy AI Gateway) reached through the model spec `gateway:<alias>`.
#[derive(Debug, Clone, Deserialize)]
pub struct LlmGatewayConfig {
    /// Base URL the engines append `/chat/completions` to; a bare host gets `/v1` appended.
    pub url: String,
    /// Env var holding the default (virtual) key.
    #[serde(default = "default_gateway_key_env")]
    pub api_key_env: String,
    /// Team name to the env var holding that team's virtual key; teams without an entry use the default key.
    #[serde(default)]
    pub team_keys: BTreeMap<String, String>,
    /// Sends `user` and `metadata` (team, application, use_case, agent, prompt_version, request_id, tags) with each request.
    #[serde(default = "default_true", deserialize_with = "lenient_bool")]
    pub send_metadata: bool,
    #[serde(default = "default_gateway_timeout")]
    pub timeout_secs: u64,
}

fn default_gateway_key_env() -> String {
    "LLM_GATEWAY_API_KEY".into()
}
fn default_true() -> bool {
    true
}
fn default_gateway_timeout() -> u64 {
    120
}

#[derive(Debug, Clone, Deserialize)]
pub struct PlatformConfig {
    pub app: String,
    #[serde(default = "default_listen")]
    pub listen: String,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default = "default_users_file")]
    pub users_file: PathBuf,
    #[serde(default = "default_intents_file")]
    pub intents_file: PathBuf,
    #[serde(default = "default_policies_file")]
    pub policies_file: PathBuf,
    #[serde(default = "default_prompts_dir")]
    pub prompts_dir: PathBuf,
    #[serde(default = "default_state_file")]
    pub prompt_state_file: PathBuf,
    pub models: ModelsConfig,
    /// Enables the `gateway:<alias>` model spec.
    #[serde(default)]
    pub llm_gateway: Option<LlmGatewayConfig>,
    /// Name of the registered agent engine; a specialist may override it.
    #[serde(default = "default_engine")]
    pub engine: String,
    #[serde(default)]
    pub pricing: BTreeMap<String, Price>,
    #[serde(default)]
    pub guard: GuardConfig,
    #[serde(default)]
    pub display: DisplayConfig,
    #[serde(default)]
    pub rate_limit: RateLimitConfig,
    #[serde(default)]
    pub routing: RoutingConfig,
    #[serde(default)]
    pub tool_visibility: ToolVisibility,
    #[serde(default)]
    pub mcp_servers: BTreeMap<String, McpServerConfig>,
    /// Remote A2A agents, selectable as `a2a:<name>` by the `a2a` engine.
    #[serde(default)]
    pub remote_agents: BTreeMap<String, RemoteAgentConfig>,
    #[serde(default)]
    pub a2a: A2aConfig,
    /// Accepted values of a caller-supplied `application`; defaults to `[app]`.
    #[serde(default)]
    pub applications: Option<Vec<String>>,
    /// Accepted values of a caller-supplied `use_case`; defaults to the registered specialist names.
    #[serde(default)]
    pub use_cases: Option<Vec<String>>,
    /// When non-empty, `/metrics` is served only on this address and not on the main listener.
    #[serde(default)]
    pub metrics_listen: Option<String>,
}

impl PlatformConfig {
    /// Delegated tokens must never be replayable against the gateway or MCP servers.
    pub fn validate(&self) -> Result<()> {
        for (name, remote) in &self.remote_agents {
            let aud = remote.audience.trim();
            if aud.is_empty() || aud == AUD_GATEWAY || aud == AUD_MCP {
                return Err(PlatformError::Config(format!(
                    "remote_agents.{name}.audience must be non-empty and not `{AUD_GATEWAY}` or `{AUD_MCP}`"
                )));
            }
        }
        if let Some(g) = &self.llm_gateway {
            if !(g.url.starts_with("http://") || g.url.starts_with("https://")) {
                return Err(PlatformError::Config(format!(
                    "llm_gateway.url must start with http:// or https://, got `{}`",
                    g.url
                )));
            }
            if g.api_key_env.trim().is_empty() || g.team_keys.values().any(|v| v.trim().is_empty())
            {
                return Err(PlatformError::Config(
                    "llm_gateway key env var names must be non-empty".into(),
                ));
            }
            if g.timeout_secs == 0 {
                return Err(PlatformError::Config(
                    "llm_gateway.timeout_secs must be at least 1".into(),
                ));
            }
        }
        if self.display.max_attempts == 0 {
            return Err(PlatformError::Config(
                "display.max_attempts must be at least 1".into(),
            ));
        }
        if self.a2a.server.task_ttl_secs == 0 {
            return Err(PlatformError::Config(
                "a2a.server.task_ttl_secs must be at least 1".into(),
            ));
        }
        Ok(())
    }

    pub fn metrics_listen(&self) -> Option<&str> {
        self.metrics_listen.as_deref().filter(|a| !a.is_empty())
    }
}

/// `^[a-z0-9][a-z0-9_.-]{0,63}$`
pub fn is_valid_use_case(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && s.len() <= 64
        && chars
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '.' | '-'))
}

fn default_listen() -> String {
    "0.0.0.0:8080".into()
}
fn default_engine() -> String {
    "rig".into()
}
fn default_users_file() -> PathBuf {
    "users.yaml".into()
}
fn default_intents_file() -> PathBuf {
    "intents.yaml".into()
}
fn default_policies_file() -> PathBuf {
    "policies.yaml".into()
}
fn default_prompts_dir() -> PathBuf {
    "prompts".into()
}
fn default_state_file() -> PathBuf {
    "./data/prompt-state.json".into()
}

#[derive(Debug, Clone, Deserialize)]
pub struct User {
    pub id: String,
    pub api_key: String,
    pub team: String,
    #[serde(default)]
    pub roles: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct UsersFile {
    users: Vec<User>,
}

#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub platform: PlatformConfig,
    pub users: Vec<User>,
    pub intents: IntentConfig,
    pub policy: Policy,
}

fn resolve(base: &Path, p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_owned()
    } else {
        base.join(p)
    }
}

fn read(path: &Path) -> Result<String> {
    std::fs::read_to_string(path)
        .map_err(|e| PlatformError::Config(format!("reading {}: {e}", path.display())))
}

impl LoadedConfig {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let base = path.parent().unwrap_or_else(|| Path::new("."));
        let mut platform: PlatformConfig = serde_yaml::from_str(&expand_env(&read(path)?)?)?;

        platform.users_file = resolve(base, &platform.users_file);
        platform.intents_file = resolve(base, &platform.intents_file);
        platform.policies_file = resolve(base, &platform.policies_file);
        platform.prompts_dir = resolve(base, &platform.prompts_dir);
        platform.prompt_state_file = resolve(base, &platform.prompt_state_file);

        let users =
            serde_yaml::from_str::<UsersFile>(&expand_env(&read(&platform.users_file)?)?)?.users;
        let intents = serde_yaml::from_str(&read(&platform.intents_file)?)?;
        let policy = Policy::load(&platform.policies_file)?;
        platform.validate()?;
        Ok(Self {
            platform,
            users,
            intents,
            policy,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_with_env_defaults_and_relative_paths() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        std::fs::write(
            p.join("platform.yaml"),
            r#"
app: demo
listen: ${AGENTIC_TEST_UNSET_LISTEN:-127.0.0.1:9999}
prompt_state_file: ./data/state.json
models: {default: "ollama:x"}
pricing: {"openai:gpt": {input: 1.0, output: 2.0}}
routing: {unclassified: llm}
tool_visibility: filtered
mcp_servers:
  docs: {url: "${AGENTIC_TEST_UNSET_URL:-http://localhost:3001/mcp}"}
"#,
        )
        .unwrap();
        std::fs::write(
            p.join("users.yaml"),
            "users:\n  - {id: alice, api_key: \"${AGENTIC_TEST_UNSET_KEY:-alice-key}\", team: t, roles: [reader]}\n",
        )
        .unwrap();
        std::fs::write(
            p.join("intents.yaml"),
            "agents:\n  a:\n    keywords: {docs: 2}\n",
        )
        .unwrap();
        std::fs::write(
            p.join("policies.yaml"),
            "default: deny\nroles:\n  reader: {allow: [\"docs.*\"]}\n",
        )
        .unwrap();

        let c = LoadedConfig::load(p.join("platform.yaml")).unwrap();
        assert_eq!(c.platform.listen, "127.0.0.1:9999");
        assert_eq!(
            c.platform.mcp_servers["docs"].url,
            "http://localhost:3001/mcp"
        );
        assert_eq!(c.platform.routing.unclassified, UnclassifiedMode::Llm);
        assert_eq!(c.platform.tool_visibility, ToolVisibility::Filtered);
        assert_eq!(c.platform.prompt_state_file, p.join("./data/state.json"));
        assert_eq!(c.platform.prompts_dir, p.join("prompts"));
        assert_eq!(c.platform.models.coordinator(), "ollama:x");
        assert_eq!(c.platform.auth.issuer, "agentic-platform");
        assert_eq!(c.platform.guard.max_attempts, 3);
        assert_eq!(c.users[0].api_key, "alice-key");
        assert_eq!(c.platform.pricing["openai:gpt"].output, 2.0);
        assert!(c.intents.agents.contains_key("a"));
    }

    #[test]
    fn a2a_config_defaults_and_lenient_bool() {
        let c: A2aConfig = serde_yaml::from_str("server: {enabled: \"true\"}").unwrap();
        assert!(c.server.enabled);
        assert_eq!(c.server.path, "/a2a");
        assert_eq!(
            c.server.audiences("minimal"),
            ["gateway", "a2a", "a2a:minimal"]
        );
        assert_eq!(c.server.task_ttl_secs, 900);
        let c: A2aConfig = serde_yaml::from_str("server: {enabled: false}").unwrap();
        assert!(!c.server.enabled);
        assert!(!A2aConfig::default().server.enabled);
        let r: RemoteAgentConfig = serde_yaml::from_str("url: http://x").unwrap();
        assert_eq!((r.audience.as_str(), r.timeout_secs), ("a2a", 120));
        assert_eq!(r.auth, RemoteAuth::Delegated);
        assert!(r.delegate_roles.is_none());
        let c: A2aConfig =
            serde_yaml::from_str("server: {accept_audiences: [a2a:x], task_ttl_secs: 5}").unwrap();
        assert_eq!(c.server.audiences("app"), ["a2a:x"]);
        assert_eq!(c.server.task_ttl_secs, 5);
    }

    #[test]
    fn remote_audience_must_not_be_a_platform_audience() {
        let cfg = |aud: &str| -> PlatformConfig {
            serde_yaml::from_str(&format!(
                "app: x\nmodels: {{default: \"ollama:x\"}}\nremote_agents:\n  r: {{url: http://r, audience: \"{aud}\"}}\n"
            ))
            .unwrap()
        };
        for bad in ["gateway", "mcp", "", " "] {
            let e = cfg(bad).validate().unwrap_err().to_string();
            assert!(e.contains("remote_agents.r.audience"), "{bad}: {e}");
        }
        for ok in ["a2a", "a2a:ops", "custom"] {
            cfg(ok).validate().unwrap();
        }
    }

    #[test]
    fn llm_gateway_block_defaults_and_validation() {
        let cfg = |y: &str| -> PlatformConfig {
            serde_yaml::from_str(&format!("app: x\nmodels: {{default: \"ollama:x\"}}\n{y}"))
                .unwrap()
        };
        assert!(cfg("").llm_gateway.is_none());
        let c = cfg("llm_gateway: {url: \"http://g:4000\"}");
        let g = c.llm_gateway.as_ref().unwrap();
        assert_eq!(g.api_key_env, "LLM_GATEWAY_API_KEY");
        assert!(g.send_metadata && g.team_keys.is_empty());
        assert_eq!(g.timeout_secs, 120);
        c.validate().unwrap();

        let c = cfg(
            "llm_gateway: {url: \"http://g\", send_metadata: \"false\", team_keys: {docs-team: K}}",
        );
        assert!(!c.llm_gateway.as_ref().unwrap().send_metadata);
        c.validate().unwrap();

        for bad in [
            "llm_gateway: {url: \"g:4000\"}",
            "llm_gateway: {url: \"\"}",
            "llm_gateway: {url: \"http://g\", team_keys: {t: \"\"}}",
            "llm_gateway: {url: \"http://g\", timeout_secs: 0}",
        ] {
            assert!(cfg(bad).validate().is_err(), "{bad}");
        }
    }

    #[test]
    fn use_case_pattern() {
        for ok in ["a", "0", "docs_agent", "a.b-c_d", &"a".repeat(64)] {
            assert!(is_valid_use_case(ok), "{ok}");
        }
        for bad in [
            "",
            "A",
            "_a",
            "-a",
            ".a",
            "a b",
            "a/b",
            "a\"b",
            "é",
            &"a".repeat(65),
        ] {
            assert!(!is_valid_use_case(bad), "{bad}");
        }
    }

    #[test]
    fn missing_env_without_default_fails() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("platform.yaml"),
            "app: ${AGENTIC_TEST_SURELY_UNSET}\n",
        )
        .unwrap();
        assert!(LoadedConfig::load(dir.path().join("platform.yaml")).is_err());
    }
}
