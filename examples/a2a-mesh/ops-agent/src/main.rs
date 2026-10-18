use std::env;

use std::time::Duration;

use anyhow::Context;
use ops_agent::{auth, model::build_model, retention::DEFAULT_TTL_SECS, server::build_router};
use tracing_subscriber::EnvFilter;

fn var(k: &str) -> Option<String> {
    env::var(k).ok().filter(|v| !v.is_empty())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let spec = var("OPS_MODEL").unwrap_or_else(|| "ollama:gpt-oss".into());
    let listen = var("OPS_LISTEN").unwrap_or_else(|| "0.0.0.0:8090".into());
    let public_url = var("OPS_PUBLIC_URL").unwrap_or_else(|| "http://localhost:8090/".into());

    let bearer = auth::from_env(&var)?;

    let ttl_secs =
        match var("OPS_TASK_TTL_SECS") {
            Some(v) => v.parse::<u64>().ok().filter(|n| *n > 0).with_context(|| {
                format!("OPS_TASK_TTL_SECS must be a positive integer, got `{v}`")
            })?,
            None => DEFAULT_TTL_SECS,
        };
    let auth_on = bearer.is_some();
    let model = build_model(&spec, &var)?;
    let app = build_router(model, &public_url, bearer, Duration::from_secs(ttl_secs)).await?;
    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    tracing::info!(%listen, %public_url, model = %spec, bearer = auth_on, "ops-agent serving A2A v1");
    axum::serve(listener, app).await?;
    Ok(())
}
