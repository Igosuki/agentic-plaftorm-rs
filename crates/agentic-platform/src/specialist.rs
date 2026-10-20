use std::{future::Future, marker::PhantomData, pin::Pin, sync::Arc};

use serde_json::Value;

use crate::guard::{self, AgentOutput, GuardRequest, GuardResult};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub struct Specialist<T> {
    name: String,
    description: String,
    prompt_id: String,
    mcp_servers: Vec<String>,
    max_turns: usize,
    engine: Option<String>,
    _output: PhantomData<fn() -> T>,
}

impl<T: AgentOutput> Specialist<T> {
    /// The prompt registry id defaults to `name`.
    pub fn new(name: impl Into<String>) -> Self {
        let name = name.into();
        Self {
            prompt_id: name.clone(),
            name,
            description: String::new(),
            mcp_servers: Vec::new(),
            max_turns: 8,
            engine: None,
            _output: PhantomData,
        }
    }

    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    pub fn prompt(mut self, prompt_id: impl Into<String>) -> Self {
        self.prompt_id = prompt_id.into();
        self
    }

    /// Name of a server in `platform.yaml` `mcp_servers`; its tools are offered to this specialist.
    pub fn mcp_server(mut self, server: impl Into<String>) -> Self {
        self.mcp_servers.push(server.into());
        self
    }

    /// Model-call budget per guard attempt, tool round-trips included.
    pub fn max_turns(mut self, max_turns: usize) -> Self {
        self.max_turns = max_turns;
        self
    }

    /// Overrides the platform `engine` for this specialist.
    pub fn engine(mut self, engine: impl Into<String>) -> Self {
        self.engine = Some(engine.into());
        self
    }

    pub(crate) fn erase(self) -> SpecialistDef {
        SpecialistDef {
            name: self.name,
            description: self.description,
            prompt_id: self.prompt_id,
            mcp_servers: self.mcp_servers,
            max_turns: self.max_turns,
            engine: self.engine,
            runner: Arc::new(TypedRunner::<T>(PhantomData)),
        }
    }
}

pub(crate) trait Runner: Send + Sync {
    fn run<'a>(&'a self, req: GuardRequest<'a>) -> BoxFuture<'a, GuardResult<Value>>;
    fn output_schema(&self) -> Value;
}

struct TypedRunner<T>(PhantomData<fn() -> T>);

impl<T: AgentOutput> Runner for TypedRunner<T> {
    fn output_schema(&self) -> Value {
        guard::output_schema::<T>()
    }

    fn run<'a>(&'a self, req: GuardRequest<'a>) -> BoxFuture<'a, GuardResult<Value>> {
        Box::pin(async move {
            let r = guard::run::<T>(req).await;
            GuardResult {
                output: r.output.and_then(|o| serde_json::to_value(o).ok()),
                attempts: r.attempts,
                usage: r.usage,
            }
        })
    }
}

#[derive(Clone)]
pub struct SpecialistDef {
    pub name: String,
    pub description: String,
    pub prompt_id: String,
    pub mcp_servers: Vec<String>,
    pub max_turns: usize,
    pub engine: Option<String>,
    pub(crate) runner: Arc<dyn Runner>,
}

impl SpecialistDef {
    pub fn output_schema(&self) -> Value {
        self.runner.output_schema()
    }
}
