use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex},
    time::Duration,
};

use rig::{
    DynModel,
    operation::Completion,
    providers::{
        anthropic::Anthropic, ollama::Ollama, openai::OpenAIConfig, registry::ProviderRef,
    },
};
use serde_json::{Value, json};

use crate::{
    PlatformError, Result,
    config::{LlmGatewayConfig, Price},
    engine::Attribution,
};

pub const GATEWAY_PROVIDER: &str = "gateway";

/// The alias of a `gateway:<alias>` spec.
pub fn gateway_alias(spec: &str) -> Option<&str> {
    spec.strip_prefix("gateway:").filter(|a| !a.is_empty())
}

pub fn missing_gateway_block(spec: &str) -> PlatformError {
    PlatformError::Model(format!(
        "model spec `{spec}` needs an `llm_gateway` block in platform.yaml"
    ))
}

/// Trims trailing slashes and appends `/v1` to a bare host; engines append `/chat/completions` to the result.
pub fn normalize_gateway_url(url: &str) -> String {
    let url = url.trim().trim_end_matches('/');
    let after_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    if after_scheme.contains('/') {
        url.to_owned()
    } else {
        format!("{url}/v1")
    }
}

/// An `llm_gateway` block with its key env vars resolved.
#[derive(Clone)]
pub struct GatewaySettings {
    base_url: String,
    default_key: Option<String>,
    default_key_env: String,
    team_keys: BTreeMap<String, String>,
    send_metadata: bool,
    timeout: Duration,
}

impl std::fmt::Debug for GatewaySettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatewaySettings")
            .field("base_url", &self.base_url)
            .field("teams", &self.team_keys.keys().collect::<Vec<_>>())
            .field("send_metadata", &self.send_metadata)
            .finish_non_exhaustive()
    }
}

impl GatewaySettings {
    /// A team whose key env var is unset or empty falls back to the default key.
    pub fn resolve(cfg: &LlmGatewayConfig, env: &dyn Fn(&str) -> Option<String>) -> Self {
        let env = |k: &str| env(k).filter(|v| !v.is_empty());
        let mut team_keys = BTreeMap::new();
        for (team, var) in &cfg.team_keys {
            match env(var) {
                Some(k) => {
                    team_keys.insert(team.clone(), k);
                }
                None => tracing::warn!(
                    team,
                    env = var,
                    "llm_gateway team key is not set; falling back to the default key"
                ),
            }
        }
        Self {
            base_url: normalize_gateway_url(&cfg.url),
            default_key: env(&cfg.api_key_env),
            default_key_env: cfg.api_key_env.clone(),
            team_keys,
            send_metadata: cfg.send_metadata,
            timeout: Duration::from_secs(cfg.timeout_secs),
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// The team's key, else the default key; errors when neither exists.
    pub fn key_for(&self, team: Option<&str>) -> Result<&str> {
        team.and_then(|t| self.team_keys.get(t))
            .or(self.default_key.as_ref())
            .map(String::as_str)
            .ok_or_else(|| {
                PlatformError::Model(format!(
                    "llm_gateway key is not set: export {}",
                    self.default_key_env
                ))
            })
    }

    /// Request-body fields recording who the call is for; `None` for non-gateway specs or when disabled.
    pub fn attribution_params(&self, spec: &str, a: &Attribution) -> Option<Value> {
        (self.send_metadata && gateway_alias(spec).is_some()).then(|| attribution_params(a))
    }
}

/// Body fields LiteLLM records in its spend logs: `user` (end_user), `metadata.tags` (request_tags) and
/// `metadata.spend_logs_metadata`, the only free-form metadata key it persists; the flat copy serves other gateways.
pub fn attribution_params(a: &Attribution) -> Value {
    let mut metadata = serde_json::Map::new();
    let mut tags = Vec::new();
    for (key, tag, value) in [
        ("team", Some("team"), &a.team),
        ("application", Some("app"), &a.application),
        ("use_case", None, &a.use_case),
        ("agent", Some("agent"), &a.agent),
        ("prompt_version", None, &a.prompt_version),
        ("request_id", None, &a.request_id),
    ] {
        if value.is_empty() {
            continue;
        }
        metadata.insert(key.into(), json!(value));
        if let Some(t) = tag {
            tags.push(format!("{t}:{value}"));
        }
    }
    let spend_logs = metadata.clone();
    metadata.insert("tags".into(), json!(tags));
    metadata.insert("spend_logs_metadata".into(), Value::Object(spend_logs));
    let mut body = json!({ "metadata": metadata });
    if !a.user.is_empty() {
        body["user"] = json!(a.user);
    }
    body
}

fn gateway_model(
    spec: &str,
    alias: &str,
    settings: &GatewaySettings,
    key: &str,
) -> Result<DynModel<Completion>> {
    let http = rig_reqwest::reqwest::Client::builder()
        .timeout(settings.timeout)
        .build()
        .map_err(|e| PlatformError::Model(format!("`{spec}`: {e}")))?;
    let mut cfg = OpenAIConfig::new(key);
    cfg.base_url = settings.base_url.clone();
    Ok(cfg
        .connect(rig_reqwest::ReqwestClient::from(http))
        .chat(alias)
        .erase())
}

/// `ollama:<model>`, `anthropic:<model>`, `openai:<model>` (honours `OPENAI_BASE_URL`), or any provider id rig's registry knows.
/// `gateway:<alias>` is resolved by `ModelCache`, which owns the gateway settings.
pub fn model_from_spec(spec: &str) -> Result<DynModel<Completion>> {
    let bad = |e: &dyn std::fmt::Display| PlatformError::Model(format!("`{spec}`: {e}"));
    let (provider, model) = spec
        .split_once(':')
        .ok_or_else(|| bad(&"expected `<provider>:<model>`"))?;
    Ok(match provider {
        GATEWAY_PROVIDER => return Err(missing_gateway_block(spec)),
        "ollama" => Ollama::from_env()
            .map_err(|e| bad(&e))?
            .completion(model)
            .erase(),
        "anthropic" => Anthropic::from_env()
            .map_err(|e| bad(&e))?
            .completion(model)
            .erase(),
        "openai" => OpenAIConfig::from_env()
            .map_err(|e| bad(&e))?
            .client()
            .chat(model)
            .erase(),
        _ => ProviderRef::parse(spec)
            .map_err(|e| bad(&e))?
            .completion_model()
            .map_err(|e| bad(&e))?,
    })
}

type ModelKey = (String, Option<String>);

/// Models keyed by spec, and for `gateway:` specs also by the virtual key they send.
#[derive(Default)]
pub struct ModelCache {
    models: Mutex<HashMap<ModelKey, DynModel<Completion>>>,
    gateway: Mutex<Option<Arc<GatewaySettings>>>,
}

impl ModelCache {
    pub fn set_gateway(&self, settings: Option<Arc<GatewaySettings>>) {
        *self.gateway.lock().unwrap_or_else(|e| e.into_inner()) = settings;
    }

    pub fn gateway(&self) -> Option<Arc<GatewaySettings>> {
        self.gateway
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// `team` selects the virtual key of a `gateway:` spec.
    pub fn get(&self, spec: &str, team: Option<&str>) -> Result<DynModel<Completion>> {
        let mut models = self.models.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(m) = models.get(&(spec.to_owned(), None)) {
            return Ok(m.clone());
        }
        let (cache_key, build): (_, Box<dyn FnOnce() -> Result<DynModel<Completion>>>) =
            match gateway_alias(spec) {
                Some(alias) => {
                    let settings = self.gateway().ok_or_else(|| missing_gateway_block(spec))?;
                    let key = settings.key_for(team)?.to_owned();
                    let alias = alias.to_owned();
                    let owned_spec = spec.to_owned();
                    (
                        (spec.to_owned(), Some(key.clone())),
                        Box::new(move || gateway_model(&owned_spec, &alias, &settings, &key)),
                    )
                }
                None => {
                    let owned_spec = spec.to_owned();
                    (
                        (spec.to_owned(), None),
                        Box::new(move || model_from_spec(&owned_spec)),
                    )
                }
            };
        if let Some(m) = models.get(&cache_key) {
            return Ok(m.clone());
        }
        let m = build()?;
        models.insert(cache_key, m.clone());
        Ok(m)
    }

    pub fn insert(&self, spec: &str, model: DynModel<Completion>) {
        self.models
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((spec.to_owned(), None), model);
    }
}

#[derive(Debug, Clone, Default)]
pub struct Pricing {
    table: BTreeMap<String, Price>,
}

impl Pricing {
    pub fn new(table: BTreeMap<String, Price>) -> Self {
        Self { table }
    }

    /// Unpriced models cost 0.
    pub fn cost_usd(&self, model_spec: &str, input_tokens: u64, output_tokens: u64) -> f64 {
        self.table.get(model_spec).map_or(0.0, |p| {
            (input_tokens as f64 * p.input + output_tokens as f64 * p.output) / 1_000_000.0
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cost_per_million() {
        let p = Pricing::new(BTreeMap::from([(
            "openai:gpt-4.1-mini".to_owned(),
            Price {
                input: 0.40,
                output: 1.60,
            },
        )]));
        let c = p.cost_usd("openai:gpt-4.1-mini", 1_000_000, 500_000);
        assert!((c - 1.2).abs() < 1e-9);
        assert_eq!(p.cost_usd("unknown:model", 10, 10), 0.0);
        assert_eq!(p.cost_usd("openai:gpt-4.1-mini", 0, 0), 0.0);
    }

    fn settings(yaml: &str, env: &[(&str, &str)]) -> GatewaySettings {
        let cfg: LlmGatewayConfig = serde_yaml::from_str(yaml).unwrap();
        let env: HashMap<String, String> = env
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        GatewaySettings::resolve(&cfg, &|k| env.get(k).cloned())
    }

    #[test]
    fn gateway_url_normalisation() {
        for (input, want) in [
            ("http://localhost:4000", "http://localhost:4000/v1"),
            ("http://localhost:4000/", "http://localhost:4000/v1"),
            ("http://g/v1", "http://g/v1"),
            ("http://g/v1/", "http://g/v1"),
            (
                "https://gw.example/openai/v1",
                "https://gw.example/openai/v1",
            ),
        ] {
            assert_eq!(normalize_gateway_url(input), want, "{input}");
        }
    }

    #[test]
    fn gateway_key_selection() {
        let s = settings(
            "{url: http://g, team_keys: {docs-team: TK, ghost-team: GK}}",
            &[("LLM_GATEWAY_API_KEY", "default"), ("TK", "team-key")],
        );
        assert_eq!(s.key_for(Some("docs-team")).unwrap(), "team-key");
        assert_eq!(s.key_for(Some("ghost-team")).unwrap(), "default");
        assert_eq!(s.key_for(Some("other")).unwrap(), "default");
        assert_eq!(s.key_for(None).unwrap(), "default");

        let none = settings("{url: http://g}", &[]);
        let e = none.key_for(None).unwrap_err().to_string();
        assert!(e.contains("LLM_GATEWAY_API_KEY"), "{e}");
        let empty = settings("{url: http://g}", &[("LLM_GATEWAY_API_KEY", "")]);
        assert!(empty.key_for(Some("t")).is_err());
    }

    #[test]
    fn gateway_spec_without_block_is_a_clear_error() {
        let e = model_from_spec("gateway:x").unwrap_err().to_string();
        assert!(
            e.contains("model spec `gateway:x` needs an `llm_gateway` block in platform.yaml"),
            "{e}"
        );
        let cache = ModelCache::default();
        assert!(cache.get("gateway:x", Some("t")).is_err());
        assert_eq!(gateway_alias("gateway:x"), Some("x"));
        assert_eq!(gateway_alias("gateway:"), None);
        assert_eq!(gateway_alias("openai:x"), None);
    }

    #[test]
    fn gateway_models_are_cached_per_key() {
        let cache = ModelCache::default();
        cache.set_gateway(Some(Arc::new(settings(
            "{url: http://g, team_keys: {docs-team: TK}}",
            &[("LLM_GATEWAY_API_KEY", "d"), ("TK", "t")],
        ))));
        cache.get("gateway:a", Some("docs-team")).unwrap();
        cache.get("gateway:a", Some("other")).unwrap();
        cache.get("gateway:a", None).unwrap();
        let keys: std::collections::BTreeSet<_> = cache
            .models
            .lock()
            .unwrap()
            .keys()
            .map(|(_, k)| k.clone())
            .collect();
        assert_eq!(keys.len(), 2);
    }

    #[test]
    fn attribution_body_shape() {
        let a = Attribution {
            user: "alice".into(),
            team: "docs-team".into(),
            application: "docs-assistant".into(),
            use_case: "docs_agent".into(),
            agent: "docs_agent".into(),
            prompt_version: "v2".into(),
            request_id: "req-1".into(),
        };
        assert_eq!(
            attribution_params(&a),
            json!({
                "user": "alice",
                "metadata": {
                    "team": "docs-team", "application": "docs-assistant", "use_case": "docs_agent",
                    "agent": "docs_agent", "prompt_version": "v2", "request_id": "req-1",
                    "tags": ["team:docs-team", "app:docs-assistant", "agent:docs_agent"],
                    "spend_logs_metadata": {
                        "team": "docs-team", "application": "docs-assistant", "use_case": "docs_agent",
                        "agent": "docs_agent", "prompt_version": "v2", "request_id": "req-1"
                    }
                }
            })
        );
        let sparse = attribution_params(&Attribution {
            agent: "coordinator".into(),
            ..Default::default()
        });
        assert_eq!(
            sparse,
            json!({"metadata": {
                "agent": "coordinator", "tags": ["agent:coordinator"],
                "spend_logs_metadata": {"agent": "coordinator"}
            }})
        );
    }

    #[test]
    fn metadata_only_for_gateway_specs_when_enabled() {
        let on = settings("{url: http://g}", &[]);
        let off = settings("{url: http://g, send_metadata: false}", &[]);
        let a = Attribution::default();
        assert!(on.attribution_params("gateway:x", &a).is_some());
        assert!(on.attribution_params("openai:x", &a).is_none());
        assert!(off.attribution_params("gateway:x", &a).is_none());
    }

    #[test]
    fn bad_spec_is_error() {
        assert!(matches!(
            model_from_spec("nocolon"),
            Err(PlatformError::Model(_))
        ));
    }
}
