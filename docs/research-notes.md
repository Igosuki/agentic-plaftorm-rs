
Research: rig 0.43 API shape
Question: how to use rig for provider-agnostic agents, tools, extractor, MCP
Recommendation: depend on facade `rig = "=0.43.0"` (features=["rmcp"]; default gives reqwest+agent+derive). 0.43 is a rewrite: Agent is non-generic, built via `AgentBuilder::new(impl Into<DynModel<Completion>>)`; MCP is crate rig-rmcp on rmcp 2.x. Probe compiled: /tmp/claude-1001/-home-geps-dev-learn-ai-harness-platform-engineering-rust-platform/a60bf14a-37d6-4ae4-82ff-f021213e8c1d/scratchpad/rig-probe (needs RUST_MIN_STACK=268435456 on nightly 1.100 or rustc SIGSEGVs on rmcp; stable 1.98 untested)
Sources: ~/.cargo/registry/src/*/rig-{core,agent,rmcp}-0.43.0, rmcp-2.2.0

Research: fastmcp-rust 0.11.0 for Dockerised MCP tool servers
Question: Can tokio/rig (rmcp streamable HTTP) agents call fastmcp-rust servers with Bearer auth?
Recommendation: Compiles from crates.io on nightly (no patches), HTTP+Bearer auth works via AuthProvider (verified with curl). BUT rmcp 2.2.0 streamable-HTTP client FAILS against it (initialize -> -32602 "request params must carry an _meta object"): fastmcp serves only stateless 2026-07-28 at POST /mcp plus legacy 2024-11-05 SSE at /sse + /messages. Prefer rmcp-based servers (rmcp server) for rig interop unless a custom client is acceptable.
Sources: scratchpad/fastmcp-probe, scratchpad/rmcp-probe; fastmcp-server/src/lib.rs (bind_http, HttpServerConfig), fastmcp-server/src/auth.rs

Research: adk-rust 2.2.0 as an optional engine
Recommendation: pin adk-core/adk-agent/adk-runner/adk-session/adk-model =2.2.0 (adk-model default-features=false, features ollama/openai/anthropic/gemini). Implement `adk_core::Tool` directly for runtime tools (`FunctionTool` only takes schemas from Rust types). Run via `Runner` + `InMemorySessionService`, `StreamingMode::None`; same session id across attempts keeps history. Sum `llm_response.usage_metadata` (prompt_token_count/candidates_token_count) over non-partial events; usage of completed calls survives an error. Max-iteration exhaustion is `AdkError::agent("Max iterations (N) exceeded")`, so no dedicated code (match is_agent + message). Provider failures can also arrive as Ok(Event) with `llm_response.error_code`. adk-model reads no Ollama env var; pass the host explicitly. Probe ran against Ollama gpt-oss: two turns with tool use and history.
Sources: ~/.cargo/registry/src/*/adk-{core,agent,runner,model,session}-2.2.0; probe at session scratchpad adk-probe

Research: A2A with a2a-rs 0.10.0 and adk-server 2.2.0
Recommendation: use a2a-rs (default-features=false, jsonrpc-server + jsonrpc-client) on both sides of the gateway. It speaks A2A v1.0: card at /.well-known/agent-card.json, PascalCase JSON-RPC (`SendMessage`), and `jsonrpc_router` nests into axum 0.8. adk-server's `create_app_with_a2a` speaks the legacy 0.3 dialect (/.well-known/agent.json, `message/send`), which cannot be bridged. Its `a2a-v1` feature handlers interoperate with a2a-rs through a shim of about 25 lines: inject `configuration.acceptedOutputModes`, and wrap the bare Task result as `{"task": ...}`. Runner needs `StreamingMode::None` or artifact text doubles. adk's remote A2A clients cannot send an Authorization header. a2a-rs DataPart numbers round-trip as floats (protobuf Value). The a2a-rs client reports a non-JSON 401 as an opaque decode error.
Sources: ~/.cargo/registry/src/*/a2a-rs-0.10.0, adk-server-2.2.0 (a2a, a2a/v1); probe at session scratchpad a2a-probe
