use agentic_core::auth::JwtKeys;
use agentic_core::policy::Policy;
use asupersync::runtime::{RuntimeBuilder, reactor::create_reactor};
use fastmcp_rust::auto::ServerBuilder;
use fastmcp_rust::prelude::*;

use crate::{JwtAuthProvider, McpServerConfig, PolicyMiddleware};

fn load_policy(config: &McpServerConfig) -> Result<Policy, String> {
    match &config.policy_file {
        Some(path) => {
            Policy::load(path).map_err(|e| format!("POLICY_FILE {}: {e}", path.display()))
        }
        None => {
            log::warn!("POLICY_FILE not set: every tools/call will be denied");
            Ok(Policy::default())
        }
    }
}

/// Serves until the listener fails. `build` registers tools on the pre-wired builder.
///
/// Configuration problems are returned; the process should exit non-zero on `Err`.
pub fn run(
    config: McpServerConfig,
    build: impl FnOnce(ServerBuilder) -> ServerBuilder,
) -> Result<(), String> {
    let _ = env_logger::try_init_from_env(env_logger::Env::default().default_filter_or("info"));
    let policy = load_policy(&config)?;
    let auth = JwtAuthProvider::new(
        JwtKeys::from_secret(config.jwt_secret.as_bytes()),
        &config.issuer,
    );
    let builder = ServerBuilder::new(config.name.clone(), env!("CARGO_PKG_VERSION"))
        .auth_provider(auth)
        .middleware(PolicyMiddleware::new(policy, config.name.clone()));
    let server = build(builder).build();

    let runtime = RuntimeBuilder::current_thread()
        .with_reactor(create_reactor().map_err(|e| format!("reactor: {e}"))?)
        .blocking_threads(0, 16)
        .build()
        .map_err(|e| format!("runtime: {e}"))?;
    log::info!("{} listening on {}", config.name, config.listen);
    runtime.block_on(async {
        let cx = Cx::current().ok_or("no runtime context")?;
        server
            .serve_http(&cx, &config.listen)
            .await
            .map(|_| ())
            .map_err(|e| format!("serve: {e}"))
    })
}
