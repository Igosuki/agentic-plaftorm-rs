#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

[ -f .env ] && { set -a; . ./.env; set +a; }
export PLATFORM_JWT_SECRET="${PLATFORM_JWT_SECRET:-$(openssl rand -hex 32)}"
export JWT_ISSUER=agentic-platform
export RUST_LOG="${RUST_LOG:-info}"
export OLLAMA_API_BASE_URL="${OLLAMA_API_BASE_URL:-http://localhost:11434}"
export OLLAMA_API_BASE_URL="${OLLAMA_API_BASE_URL/host.docker.internal/localhost}"

DOCS_PORT="${DOCS_PORT:-3001}"
CALC_PORT="${CALC_PORT:-3003}"
OPS_DIR=examples/a2a-mesh/ops-agent
MESH_DIR=examples/a2a-mesh/mesh-gateway

pids=()
trap 'kill "${pids[@]}" 2>/dev/null || true; wait 2>/dev/null || true' EXIT INT TERM

cargo build --manifest-path mcp/Cargo.toml -p docs-mcp -p calc-mcp
cargo build -p minimal --features a2a
cargo build -p mesh-gateway
cargo build --manifest-path "$OPS_DIR/Cargo.toml"

MCP_SERVER_NAME=docs MCP_LISTEN=127.0.0.1:$DOCS_PORT POLICY_FILE=$MESH_DIR/config/policies.yaml \
  mcp/target/debug/docs-mcp & pids+=($!)
MCP_SERVER_NAME=calc MCP_LISTEN=127.0.0.1:$CALC_PORT POLICY_FILE=examples/minimal/config/policies.yaml \
  mcp/target/debug/calc-mcp & pids+=($!)

OPS_LISTEN=127.0.0.1:8090 OPS_PUBLIC_URL=http://localhost:8090/ OPS_JWT_AUDIENCE=a2a:ops \
  "$OPS_DIR/target/debug/ops-agent" & pids+=($!)

for url in http://127.0.0.1:$DOCS_PORT/health http://127.0.0.1:$CALC_PORT/health http://127.0.0.1:8090/health; do
  until curl -fsS "$url" >/dev/null 2>&1; do sleep 0.5; done
done

LISTEN_ADDR=127.0.0.1:8081 A2A_ENABLED=true A2A_PUBLIC_URL=http://localhost:8081/a2a \
CALC_MCP_URL=http://127.0.0.1:$CALC_PORT/mcp PLATFORM_CONFIG=examples/minimal/config/platform.yaml \
  target/debug/minimal & pids+=($!)

until curl -fsS http://127.0.0.1:8081/healthz >/dev/null 2>&1; do sleep 0.5; done

LISTEN_ADDR=127.0.0.1:8082 A2A_PUBLIC_URL=http://localhost:8082/a2a \
DOCS_MCP_URL=http://127.0.0.1:$DOCS_PORT/mcp OPS_AGENT_URL=http://127.0.0.1:8090 \
MINIMAL_A2A_URL=http://127.0.0.1:8081/a2a PLATFORM_CONFIG=$MESH_DIR/config/platform.yaml \
  target/debug/mesh-gateway & pids+=($!)

echo "mesh up: gateway http://localhost:8082, minimal http://localhost:8081, ops-agent http://localhost:8090"
wait -n
