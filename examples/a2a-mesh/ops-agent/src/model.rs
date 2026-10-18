use std::sync::Arc;

use adk_core::Llm;
use adk_model::{
    GeminiModel, OllamaConfig, OllamaModel,
    anthropic::{AnthropicClient, AnthropicConfig},
    openai::OpenAIClient,
};
use anyhow::{Result, anyhow};

const PROVIDERS: &str = "ollama, openai, anthropic, gemini";
const DEFAULT_OLLAMA_HOST: &str = "http://localhost:11434";

pub fn build_model(spec: &str, env: &dyn Fn(&str) -> Option<String>) -> Result<Arc<dyn Llm>> {
    let bad = |e: &dyn std::fmt::Display| anyhow!("model `{spec}`: {e}");
    let env = |k: &str| env(k).filter(|v| !v.is_empty());
    let (provider, model) = spec.split_once(':').ok_or_else(|| {
        bad(&format_args!(
            "expected `<provider>:<model>` with provider one of: {PROVIDERS}"
        ))
    })?;
    let key = |name: &str| env(name).ok_or_else(|| bad(&format_args!("{name} is not set")));
    Ok(match provider {
        "ollama" => {
            let host = env("OLLAMA_API_BASE_URL")
                .or_else(|| env("OLLAMA_HOST"))
                .unwrap_or_else(|| DEFAULT_OLLAMA_HOST.into());
            Arc::new(OllamaModel::new(OllamaConfig::with_host(host, model)).map_err(|e| bad(&e))?)
        }
        "openai" => {
            // Compatible servers usually ignore the key, but the client requires one.
            let (api_key, base) = match env("OPENAI_BASE_URL") {
                Some(base) => (
                    env("OPENAI_API_KEY").unwrap_or_else(|| "unused".into()),
                    base,
                ),
                None => (key("OPENAI_API_KEY")?, "https://api.openai.com/v1".into()),
            };
            Arc::new(OpenAIClient::compatible(api_key, base, model).map_err(|e| bad(&e))?)
        }
        "anthropic" => {
            let mut cfg = AnthropicConfig::new(key("ANTHROPIC_API_KEY")?, model);
            if let Some(base) = env("ANTHROPIC_BASE_URL") {
                cfg = cfg.with_base_url(base);
            }
            Arc::new(AnthropicClient::new(cfg).map_err(|e| bad(&e))?)
        }
        "gemini" => Arc::new(GeminiModel::from_env(model).map_err(|e| bad(&e))?),
        other => {
            return Err(bad(&format_args!(
                "unsupported provider `{other}`; supported: {PROVIDERS}"
            )));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none(_: &str) -> Option<String> {
        None
    }

    fn err(spec: &str, env: &dyn Fn(&str) -> Option<String>) -> String {
        match build_model(spec, env) {
            Ok(_) => panic!("{spec} should not build"),
            Err(e) => e.to_string(),
        }
    }

    #[test]
    fn ollama_builds_without_env() {
        assert!(build_model("ollama:gpt-oss", &none).is_ok());
    }

    #[test]
    fn ollama_prefers_api_base_url() {
        let env = |k: &str| (k == "OLLAMA_API_BASE_URL").then(|| "http://example:1".to_string());
        assert!(build_model("ollama:gpt-oss", &env).is_ok());
    }

    #[test]
    fn missing_provider_separator() {
        assert!(err("gpt-oss", &none).contains("<provider>:<model>"));
    }

    #[test]
    fn unknown_provider() {
        assert!(err("foo:bar", &none).contains("unsupported provider `foo`"));
    }

    #[test]
    fn openai_needs_key_without_base_url() {
        assert!(err("openai:gpt-4o", &none).contains("OPENAI_API_KEY"));
        let env = |k: &str| (k == "OPENAI_BASE_URL").then(|| "http://localhost:1/v1".to_string());
        assert!(build_model("openai:gpt-4o", &env).is_ok());
    }

    #[test]
    fn anthropic_needs_key() {
        assert!(err("anthropic:claude", &none).contains("ANTHROPIC_API_KEY"));
    }
}
