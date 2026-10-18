# ops-agent

A standalone [adk-rust](https://crates.io/crates/adk-rust) `LlmAgent` served over A2A v1. It plays "another team's agent built with a different framework": the platform's a2a-rs client calls it like any remote agent.

The agent answers Nimbus Queue (fictional message queue) incident questions. It looks up embedded runbooks through two local tools (`list_runbooks`, `get_runbook`) and replies with only a JSON object:

```json
{"summary": "...", "runbook_id": "rb-consumer-lag" | null, "severity": "low|medium|high", "steps": ["..."]}
```

Runbook ids: `rb-consumer-lag`, `rb-disk-full`, `rb-broker-down`, `rb-dlq-growth`.

This is its own Cargo workspace (excluded from the root workspace) because adk-server pulls reqwest 0.12 and enables serde_json `preserve_order`, which the platform workspace avoids.

## Run

```sh
# needs a local Ollama with gpt-oss pulled; bearer auth is on by default, so a secret is required
export PLATFORM_JWT_SECRET=$(openssl rand -hex 32)
cargo run --release
cargo run --release --example ask -- http://localhost:8090 "Consumer lag on orders is growing, what do I do?"
# local experiments without tokens: OPS_REQUIRE_BEARER=0 cargo run --release
```

The `ask` example is an a2a-rs 0.10 client (same library as the platform). It prints the card, task state, final text and the parsed JSON. If `PLATFORM_JWT_SECRET` is set it mints an HS256 token (`sub=alice`, `team=docs-team`, `roles=[reader]`, `aud=$AUDIENCE`, default `a2a`; use the server's `OPS_JWT_AUDIENCE`); `TOKEN` overrides with a ready-made one. Without either it sends no token, which only works against `OPS_REQUIRE_BEARER=0`.

## Environment

| Variable | Default | Meaning |
| --- | --- | --- |
| `OPS_MODEL` | `ollama:gpt-oss` | `ollama:`, `openai:`, `anthropic:` or `gemini:` model spec |
| `OPS_LISTEN` | `0.0.0.0:8090` | bind address |
| `OPS_PUBLIC_URL` | `http://localhost:8090/` | URL advertised in the agent card |
| `OPS_REQUIRE_BEARER` | on | `Authorization: Bearer <jwt>` is required on `POST /`; `0` disables it and logs a startup warning |
| `PLATFORM_JWT_SECRET` | | HS256 secret, required unless `OPS_REQUIRE_BEARER=0` |
| `OPS_JWT_AUDIENCE` | `a2a` | expected `aud` (issuer is fixed to `agentic-platform`); the mesh sets `a2a:ops`, matching the gateway's `remote_agents.ops.audience` |
| `OPS_TASK_TTL_SECS` | `900` | finished tasks (and their ADK sessions) idle longer than this are deleted; a sweep runs every `min(ttl/4, 60s)` |
| `OLLAMA_API_BASE_URL` / `OLLAMA_HOST` | `http://localhost:11434` | Ollama host, in that order |
| `OPENAI_API_KEY`, `OPENAI_BASE_URL`, `ANTHROPIC_API_KEY`, `ANTHROPIC_BASE_URL`, `GEMINI_API_KEY` | | provider credentials |
| `RUST_LOG` | `info` | log filter |

Routes: `POST /` (JSON-RPC), `GET /.well-known/agent-card.json` (public), `GET /health`. adk-server has no auth, so the bearer layer is ours; the card and health routes stay public. adk-server has no per-caller task ownership either: any caller with a valid token can read any task by id, which is acceptable for this single-tenant demo agent but not for multi-tenant use.

## adk-server v1 wire deviations and the shim

adk-server's `a2a-v1` feature (a2a-protocol-types 0.5) is not wire-compatible with a2a-rs 0.10 in two ways. `src/shim.rs` is a middleware around the JSON-RPC route that fixes both:

1. adk requires `params.configuration.acceptedOutputModes`; a2a-rs omits it. The shim injects `[]` when missing.
2. For `SendMessage`, adk returns a bare Task as `result`; the v1 oneof (and a2a-rs) expects `{"task": {...}}`. The shim wraps it, and leaves already-wrapped results and errors alone.

The shim buffers bodies: a request over 8 MiB is answered with HTTP 413 and a response over 32 MiB with 502, never an empty body.

## Design notes

- `create_app_with_a2a` is not used: it serves the legacy 0.3 dialect (`/.well-known/agent.json`, `message/send`), which cannot be bridged to an a2a-rs v1 client. The v1 handlers (`RequestHandler`, `V1Executor`, `jsonrpc_handler`) are wired by hand instead.
- The runner uses `StreamingMode::None`. With streaming enabled the final artifact text is emitted twice.
- Temperature is 0 and the instruction forbids prose, so the calling platform's JSON-schema validation and re-prompting rarely triggers.

## Tests

```sh
cargo test
cargo clippy --all-targets -- -D warnings
```

If rustc segfaults on deep recursion, use `ulimit -s unlimited` and `RUST_MIN_STACK=1073741824`.
