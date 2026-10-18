# rust-platform

A reusable Rust framework for production LLM agents. It follows the architecture in InfoQ's
[Platform Engineering Playbook for Production LLMs](https://www.infoq.com/articles/platform-engineering-playbook-production-llms/),
built on tokio, axum, [rig](https://github.com/0xPlaygrounds/rig) 0.43, [fastmcp-rust](https://github.com/Dicklesworthstone/fastmcp_rust) 0.11 and docker-compose.

```
client ──JWT──▶ gateway (axum) ──▶ intent gate ──▶ coordinator ──▶ specialist (rig agent, registry prompt)
                  │ rate limit        │ keywords      │ LLM fallback      │ schema-enforced retries
                  │ cost attribution  ▼               ▼                   ▼
                  │                unclassified → 422                MCP tool servers (fastmcp-rust)
                  ▼                                                  delegated JWT, default-deny policy
             /metrics ──▶ Prometheus ──▶ Grafana
```

Each article concept maps to a specific module; the table is in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md#1-mapping-from-the-article). In short:

- **Gateway**: one ingress. It issues role-bearing JWTs, applies a per-team rate limit and tags every request with team, application and use case.
- **Intent gate**: a cheap YAML keyword gate that returns `unclassified` instead of guessing. An optional LLM coordinator handles requests the gate can't place.
- **Specialists**: typed outputs (`AgentOutput`), each with a `validate` hook that checks the answer against the run's tool evidence. This is how hallucinated doc ids or ticket ids are caught.
- **Guard**: the retry loop. Schema violations, hallucinations, max-turn exhaustion and infra errors are classified separately, and each kind gets its own retry strategy under attempt and token budgets.
- **Prompt registry**: versioned YAML prompts with a live pointer. Admins can move the pointer or roll it back at runtime without a redeploy.
- **MCP servers**: authorization is enforced at every server, not just the gateway. Each server verifies a short-lived delegated JWT (`aud: mcp`) and applies a default-deny policy per `server.tool`.
- **Observability**: tokens, cost, attempt outcomes per prompt version (to show prompt drift), tool calls and denials, intent results, plus per-user `UserMetrics`.
- **Golden-set eval**: `agentic-eval` scores routing, tool trajectory and response, and exits non-zero below the thresholds, so it works as a CI gate for prompt changes.

## Layout

| Path | What |
|---|---|
| `crates/agentic-core` | Runtime-agnostic code shared by both sides: JWT, policy, request tags, MCP wire constants, env expansion |
| `crates/agentic-platform` | The tokio framework: gateway, intent, coordinator, prompts, guard, MCP client, telemetry |
| `crates/agentic-eval` | Golden-set evaluator CLI |
| `mcp/agentic-mcp` | fastmcp-rust helpers: JWT `AuthProvider`, policy `Middleware`, `run()` |
| `mcp/servers/*` | `docs-mcp`, `tickets-mcp`, `calc-mcp` |
| `examples/docs-assistant` | The article's example: `docs_agent` (grounded doc Q&A) and `tickets_agent` (role-gated ticket creation) |
| `examples/minimal` | Copy-me template: one specialist and one MCP server |
| `deploy/` | Dockerfile, Prometheus, Grafana dashboard, `run-local.sh` |

`mcp/` is a separate Cargo workspace for two reasons:
- fastmcp-rust runs on the asupersync runtime, not tokio.
- It pins `serde_json` with `arbitrary_precision`, and that feature must not unify into rig.

fastmcp-rust serves the stateless MCP 2026-07-28 protocol, which rmcp (and therefore rig-rmcp) can't speak. The platform therefore ships its own small MCP client: `agentic_platform::mcp`.

## Quick start

You need the pinned nightly from `rust-toolchain.toml` (rustup installs it automatically), Docker, and an LLM. By default that is Ollama on the host with `ollama pull gpt-oss`.

```sh
make env        # creates .env with a random PLATFORM_JWT_SECRET
make up         # (or make up-prebuilt, see below) gateway :8080, Prometheus :9090, Grafana :3000 (all bound to 127.0.0.1)
make eval       # golden-set eval against the running gateway
```

To run without Docker, use `make run-local`. It runs both MCP servers and the gateway natively.

**Docker memory.** rustc needs about 10 GB of RAM to compile asupersync, which fastmcp-rust depends on. With Docker Desktop's default VM memory, `make up` thrashes or hangs. Either raise the VM's memory to at least 12 GB, or run `make up-prebuilt`. That target builds the release binaries on the host and only packages them into the same runtime image (`deploy/Dockerfile`, stage `prebuilt`, through the `docker-compose.prebuilt.yml` overlay).

### Engines: rig (default) or Google ADK

The agent loop sits behind an `AgentEngine` trait. `rig` is built in; [adk-rust](https://github.com/zavora-ai/adk-rust) (Google ADK for Rust) is an optional engine behind the `adk` cargo feature. Build with the feature and select the engine:

```sh
make up-prebuilt FEATURES=adk AGENT_ENGINE=adk          # host build with --features adk, then package
make run-local FEATURES=adk AGENT_ENGINE=adk            # native, no Docker
# or in .env: CARGO_FEATURES=adk and AGENT_ENGINE=adk, then `make up`
```

`trace.engine` in the response reports which engine ran. Differences are listed in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md#agent-engines). The ADK article's `AgentEvaluator` maps to the engine-agnostic `agentic-eval` (`make eval`), which works against either engine.

To switch the model provider, change `.env`; no code changes are needed:
`DEFAULT_MODEL=openai:gpt-4.1-mini`, `anthropic:claude-sonnet-5-5`, `gemini:...`, or any `ollama:<model>`.

### Try it

```sh
TOKEN=$(curl -s localhost:8080/auth/token -d '{"api_key":"alice-key"}' -H 'content-type: application/json' | jq -r .access_token)

curl -s localhost:8080/v1/agents/run -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  -d '{"input":"What is the default retention period in hours?"}' | jq

# alice is a reader: the tickets MCP server denies create_ticket and the agent reports action=denied
curl -s localhost:8080/v1/agents/run -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  -d '{"input":"Create a ticket: consumer lag on orders topic"}' | jq '.output, .trace.tool_calls'

curl -s localhost:8080/v1/metrics/me -H "authorization: Bearer $TOKEN" | jq
```

The demo users are defined in `examples/docs-assistant/config/users.yaml`:
- `alice-key` and `dave-key`: reader
- `bob-key`: engineer
- `carol-key`: admin

Each key can be overridden with a `*_API_KEY` env var.

Admins can roll prompts back:

```sh
ADMIN=$(curl -s localhost:8080/auth/token -d '{"api_key":"carol-key"}' -H 'content-type: application/json' | jq -r .access_token)
curl -s localhost:8080/admin/prompts -H "authorization: Bearer $ADMIN" | jq
curl -s -XPOST localhost:8080/admin/prompts/docs_agent/rollback -H "authorization: Bearer $ADMIN" | jq
```

To check that a candidate prompt clears the gate before promoting it, run:

```sh
cargo run -p agentic-eval -- --gateway http://localhost:8080 \
  --golden examples/docs-assistant/eval/golden.json --config examples/docs-assistant/eval/eval.yaml \
  --set-live docs_agent=1.0.0 --admin-key carol-key
```

The eval sets the candidate live, runs the golden set, then restores the previous version.

### Optional AI gateway

The engines can also reach models through a shared, OpenAI-compatible AI gateway instead of calling providers directly. The reference deployment is the LiteLLM Proxy with Postgres and Redis, in `deploy/llm-gateway/`. Model specs become aliases (`gateway:default`), provider keys live only in the gateway, and each call carries team, app, agent and `request_id` tags into the gateway's spend log.

```sh
make up-with-gateway        # LiteLLM tier + docs-assistant using DEFAULT_MODEL=gateway:default
```

[ARCHITECTURE.md section 13](docs/ARCHITECTURE.md#13-ai-gateway-optional) explains how a gateway tier improves scaling, resilience, key custody and cost governance, and what stays in the platform.

## Starting a new agentic project

Copy `examples/minimal` and follow [its checklist](examples/minimal/README.md). The reuse surface is:

```rust
#[derive(Serialize, Deserialize, JsonSchema)]
struct MyOutput { answer: String, sources: Vec<String> }

impl AgentOutput for MyOutput {
    fn validate(&self, evidence: &Evidence) -> Result<(), Violation> { /* check against tool results */ Ok(()) }
}

Platform::from_config(config_path_from_env_or_args())?
    .specialist(Specialist::<MyOutput>::new("my_agent").description("...").mcp_server("my_tools"))
    .build().await?
    .serve().await
```

Most of a project lives in YAML config: platform, users, intents, policies and prompts. Tool servers are about 10 lines each on top of `agentic_mcp::run`.

## Development

```sh
make test       # both workspaces, plus agentic-platform with the `adk` feature
make clippy     # both workspaces, -D warnings, plus the `adk` feature
```

`.cargo/config.toml` raises `RUST_MIN_STACK` because the pinned nightly overflows rustc's stack on some dependencies. If rustc still segfaults, run `ulimit -s unlimited` first.

## Known limits

- fastmcp-rust 0.11 is young and has a single maintainer. Its license (MIT with a rider) and the license of asupersync need checking before production use.
- Users and API keys are demo identities. Production should put OIDC behind the same `Claims`.
- Metrics, `UserMetrics` and the rate limiter are in-memory per gateway replica.
- The prompt-state file assumes a single writer.
