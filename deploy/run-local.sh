#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

# Values given on the command line win over .env (e.g. DEFAULT_MODEL=gateway:default make run-local).
keep=()
for v in DEFAULT_MODEL COORDINATOR_MODEL AGENT_ENGINE LLM_GATEWAY_URL LLM_GATEWAY_API_KEY; do
  [ -n "${!v:-}" ] && keep+=("$v=${!v}")
done
[ -f .env ] && { set -a; . ./.env; set +a; }
for kv in ${keep[@]+"${keep[@]}"}; do export "$kv"; done
export PLATFORM_JWT_SECRET="${PLATFORM_JWT_SECRET:-$(openssl rand -hex 32)}"
export JWT_ISSUER=agentic-platform
export POLICY_FILE=examples/docs-assistant/config/policies.yaml
export RUST_LOG="${RUST_LOG:-info}"
OLLAMA_API_BASE_URL="${OLLAMA_API_BASE_URL:-http://localhost:11434}"

# Native runs reach the dockerised gateway on its published port.
export LLM_GATEWAY_URL="${LLM_GATEWAY_URL:-http://localhost:4000}"
export LLM_GATEWAY_API_KEY="${LLM_GATEWAY_API_KEY:-${LITELLM_MASTER_KEY:-}}"

pids=()
trap 'kill "${pids[@]}" 2>/dev/null || true; wait 2>/dev/null || true' EXIT INT TERM

cargo build --manifest-path mcp/Cargo.toml -p docs-mcp -p tickets-mcp
cargo build -p docs-assistant ${FEATURES:+--features "$FEATURES"}

MCP_SERVER_NAME=docs MCP_LISTEN=127.0.0.1:3001 mcp/target/debug/docs-mcp & pids+=($!)
MCP_SERVER_NAME=tickets MCP_LISTEN=127.0.0.1:3002 mcp/target/debug/tickets-mcp & pids+=($!)

for port in 3001 3002; do
  until curl -fsS "http://127.0.0.1:$port/health" >/dev/null 2>&1; do sleep 0.5; done
done

AGENT_ENGINE="${AGENT_ENGINE:-rig}" \
DOCS_MCP_URL=http://127.0.0.1:3001/mcp \
TICKETS_MCP_URL=http://127.0.0.1:3002/mcp \
PLATFORM_CONFIG=examples/docs-assistant/config/platform.yaml \
OLLAMA_API_BASE_URL="${OLLAMA_API_BASE_URL/host.docker.internal/localhost}" \
target/debug/docs-assistant & pids+=($!)

wait -n
