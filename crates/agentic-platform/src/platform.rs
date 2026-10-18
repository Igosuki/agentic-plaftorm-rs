use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
};

use agentic_core::auth::JwtKeys;
use axum::Router;
use rig::{DynModel, operation::Completion};

use crate::{
    PlatformError, Result,
    config::{LoadedConfig, UnclassifiedMode, is_valid_use_case},
    coordinator::Coordinator,
    engine::{AgentEngine, RigEngine},
    gateway::{self, AppState, SharedState, rate_limit::RateLimiter},
    guard::AgentOutput,
    intent::IntentClassifier,
    llm::{GatewaySettings, Pricing},
    mcp::McpHub,
    prompts::PromptRegistry,
    specialist::{Specialist, SpecialistDef},
    telemetry::{self, MetricsStore},
};

/// First CLI argument, else `PLATFORM_CONFIG`, else `config/platform.yaml`.
pub fn config_path_from_env_or_args() -> PathBuf {
    std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("PLATFORM_CONFIG").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("config/platform.yaml"))
}

pub struct Platform {
    state: SharedState,
}

pub struct PlatformBuilder {
    config: LoadedConfig,
    specialists: Vec<SpecialistDef>,
    rig: Arc<RigEngine>,
    engines: HashMap<String, Arc<dyn AgentEngine>>,
    keys: Option<JwtKeys>,
}

impl Platform {
    /// Loads the config and initialises tracing and the Prometheus recorder (both once per process).
    pub fn from_config(path: impl AsRef<Path>) -> Result<PlatformBuilder> {
        telemetry::init_tracing();
        telemetry::prometheus_handle();
        Ok(PlatformBuilder {
            config: LoadedConfig::load(path)?,
            specialists: Vec::new(),
            rig: Arc::new(RigEngine::new()),
            engines: HashMap::new(),
            keys: None,
        })
    }

    pub fn state(&self) -> &SharedState {
        &self.state
    }

    /// The gateway API, plus the A2A endpoint when `a2a.server.enabled`.
    pub fn router(&self) -> Router {
        let router = gateway::router(self.state.clone());
        #[cfg(feature = "a2a")]
        if self.state.config.a2a.server.enabled {
            return router.merge(crate::a2a_server::router(&self.state));
        }
        router
    }

    /// The `/metrics`-only router served on `metrics_listen`.
    pub fn metrics_router(&self) -> Router {
        gateway::metrics_router(self.state.clone())
    }

    pub fn listen_addr(&self) -> &str {
        &self.state.config.listen
    }

    /// Binds `listen` and serves until SIGINT or SIGTERM, draining in-flight requests.
    pub async fn serve(self) -> Result<()> {
        let listener = tokio::net::TcpListener::bind(self.listen_addr()).await?;
        tracing::info!(addr = %listener.local_addr()?, app = %self.state.config.app, "gateway listening");
        self.serve_on(listener).await
    }

    /// Also binds `metrics_listen` when configured.
    pub async fn serve_on(self, listener: tokio::net::TcpListener) -> Result<()> {
        let metrics = match self.state.config.metrics_listen() {
            Some(addr) => {
                let l = tokio::net::TcpListener::bind(addr).await?;
                tracing::info!(addr = %l.local_addr()?, "metrics listening");
                Some(l)
            }
            None => None,
        };
        let main = async {
            axum::serve(
                listener,
                self.router()
                    .into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(shutdown_signal())
            .await
        };
        let metrics = async {
            match metrics {
                Some(l) => {
                    axum::serve(l, gateway::metrics_router(self.state.clone()))
                        .with_graceful_shutdown(shutdown_signal())
                        .await
                }
                None => Ok(()),
            }
        };
        tokio::try_join!(main, metrics)?;
        Ok(())
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        () = term => {},
    }
    tracing::info!("shutdown signal received");
}

impl PlatformBuilder {
    pub fn specialist<T: AgentOutput>(mut self, specialist: Specialist<T>) -> Self {
        self.specialists.push(specialist.erase());
        self
    }

    /// Registers a prebuilt model for `spec`, bypassing `model_from_spec` (tests, custom providers).
    pub fn model(self, spec: &str, model: DynModel<Completion>) -> Self {
        self.rig.insert_model(spec, model);
        self
    }

    /// Registers a custom engine selectable as `engine: <name>` in `platform.yaml` or `Specialist::engine`.
    pub fn engine(mut self, name: impl Into<String>, engine: Arc<dyn AgentEngine>) -> Self {
        self.engines.insert(name.into(), engine);
        self
    }

    /// Overrides the signing keys that are otherwise read from `auth.jwt_secret_env`.
    pub fn jwt_keys(mut self, keys: JwtKeys) -> Self {
        self.keys = Some(keys);
        self
    }

    /// Validates specialists against config, builds models, and fetches each used MCP server's tool catalog.
    pub async fn build(self) -> Result<Platform> {
        let Self {
            config,
            specialists,
            rig,
            mut engines,
            keys,
        } = self;
        let LoadedConfig {
            platform,
            users,
            intents,
            policy,
        } = config;
        let cfg_err = |m: String| PlatformError::Config(m);

        let mut by_name = BTreeMap::new();
        for s in specialists {
            if by_name.contains_key(&s.name) {
                return Err(cfg_err(format!("specialist `{}` registered twice", s.name)));
            }
            by_name.insert(s.name.clone(), s);
        }
        if by_name.is_empty() {
            return Err(cfg_err("no specialists registered".into()));
        }

        let prompts = PromptRegistry::load(&platform.prompts_dir, &platform.prompt_state_file)?;
        let mut servers = BTreeSet::new();
        for s in by_name.values() {
            if !prompts.contains(&s.prompt_id) {
                return Err(cfg_err(format!(
                    "specialist `{}`: prompt `{}` not found in {}",
                    s.name,
                    s.prompt_id,
                    platform.prompts_dir.display()
                )));
            }
            for server in &s.mcp_servers {
                if !platform.mcp_servers.contains_key(server) {
                    return Err(cfg_err(format!(
                        "specialist `{}`: mcp server `{server}` is not in platform.yaml mcp_servers",
                        s.name
                    )));
                }
                servers.insert(server.clone());
            }
        }

        let mut classifier = IntentClassifier::new(intents).map_err(cfg_err)?;
        for name in classifier.agent_names() {
            if !by_name.contains_key(name) {
                tracing::warn!(
                    agent = name,
                    "intents.yaml references an unregistered specialist"
                );
            }
        }
        classifier.retain_agents(|n| by_name.contains_key(n));

        let keys = match keys {
            Some(k) => k,
            None => JwtKeys::from_env(&platform.auth.jwt_secret_env)?,
        };
        platform.validate()?;
        let a2a_server = &platform.a2a.server;
        if a2a_server.enabled {
            if cfg!(not(feature = "a2a")) {
                return Err(cfg_err(
                    "`a2a.server.enabled` requires building agentic-platform with the `a2a` cargo feature"
                        .into(),
                ));
            }
            if !a2a_server.path.starts_with('/') || a2a_server.path.trim_end_matches('/').is_empty()
            {
                return Err(cfg_err(
                    "`a2a.server.path` must be a non-root path starting with `/`".into(),
                ));
            }
        }
        let gateway = platform
            .llm_gateway
            .as_ref()
            .map(|c| Arc::new(GatewaySettings::resolve(c, &|k| std::env::var(k).ok())));
        rig.set_gateway(gateway.clone());
        engines.entry("rig".into()).or_insert(rig);
        #[cfg(feature = "adk")]
        engines
            .entry("adk".into())
            .or_insert_with(|| Arc::new(crate::engine::AdkEngine::with_gateway(gateway)));
        #[cfg(feature = "a2a")]
        engines.entry("a2a".into()).or_insert_with(|| {
            Arc::new(crate::engine::A2aEngine::new(
                platform.app.clone(),
                platform.remote_agents.clone(),
                keys.clone(),
                platform.auth.issuer.clone(),
                platform.auth.delegated_ttl_secs,
            ))
        });
        let engine_for = |name: &str| {
            engines.get(name).cloned().ok_or_else(|| {
                if name == "adk" || name == "a2a" {
                    return cfg_err(format!(
                        "engine `{name}` requires building agentic-platform with the `{name}` cargo feature"
                    ));
                }
                let mut supported: Vec<&str> = engines.keys().map(String::as_str).collect();
                supported.sort_unstable();
                cfg_err(format!(
                    "unknown engine `{name}`; supported engines: {}",
                    supported.join(", ")
                ))
            })
        };
        let default_engine = engine_for(&platform.engine)?;
        for s in by_name.values() {
            if let Some(name) = &s.engine {
                engine_for(name).map_err(|e| cfg_err(format!("specialist `{}`: {e}", s.name)))?;
            }
        }
        default_engine.check_model(&platform.models.default)?;
        let fallback = (platform.routing.unclassified == UnclassifiedMode::Llm)
            .then(|| {
                let spec = platform.models.coordinator();
                default_engine.check_model(spec).map(|()| spec.to_owned())
            })
            .transpose()?;
        let coordinator = Coordinator::new(
            platform.app.clone(),
            classifier,
            platform.routing.unclassified,
            default_engine,
            fallback,
            by_name
                .values()
                .map(|s| (s.name.clone(), s.description.clone()))
                .collect(),
        );

        let policy = Arc::new(policy);
        let hub = McpHub::connect(
            servers.into_iter().map(|n| {
                let url = platform.mcp_servers[&n].url.clone();
                (n, url)
            }),
            keys.clone(),
            platform.auth.issuer.clone(),
            platform.auth.delegated_ttl_secs,
            policy.clone(),
            platform.tool_visibility,
        )
        .await?;

        let applications: BTreeSet<String> = platform
            .applications
            .clone()
            .unwrap_or_else(|| vec![platform.app.clone()])
            .into_iter()
            .collect();
        let use_cases: BTreeSet<String> = platform
            .use_cases
            .clone()
            .unwrap_or_else(|| by_name.keys().cloned().collect())
            .into_iter()
            .collect();
        if let Some(bad) = use_cases.iter().find(|u| !is_valid_use_case(u)) {
            return Err(cfg_err(format!(
                "use_case `{bad}` does not match ^[a-z0-9][a-z0-9_.-]{{0,63}}$"
            )));
        }

        let state = AppState {
            keys,
            users_by_key: users.into_iter().map(|u| (u.api_key.clone(), u)).collect(),
            prompts,
            coordinator,
            specialists: by_name,
            hub,
            policy,
            engines,
            pricing: Pricing::new(platform.pricing.clone()),
            limiter: RateLimiter::new(platform.rate_limit.requests_per_minute_per_team),
            auth_limiter: RateLimiter::new(platform.rate_limit.auth_rate_limit_per_minute),
            applications,
            use_cases,
            store: MetricsStore::default(),
            prometheus: telemetry::prometheus_handle(),
            config: platform,
        };
        Ok(Platform {
            state: Arc::new(state),
        })
    }
}
