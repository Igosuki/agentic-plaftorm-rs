COMPOSE := docker compose
MCP := --manifest-path mcp/Cargo.toml --workspace
ADK_PKGS := -p agentic-platform -p docs-assistant -p minimal --features docs-assistant/adk,minimal/adk
A2A_PKGS := -p agentic-platform -p minimal --features agentic-platform/a2a,minimal/a2a
ADK_A2A_PKGS := -p agentic-platform --features adk,a2a
FEATURES ?=
export FEATURES
GATEWAY_COMPOSE := $(COMPOSE) --env-file .env -f deploy/llm-gateway/docker-compose.yml

.PHONY: env build test clippy up up-prebuilt prebuilt down logs up-minimal up-mesh up-llm-gateway down-llm-gateway up-with-gateway eval eval-mesh run-local run-mesh-local

build:
	cargo build --workspace
	cargo build $(MCP)

test:
	cargo test --workspace
	cargo test $(ADK_PKGS)
	cargo test $(A2A_PKGS)
	cargo test $(ADK_A2A_PKGS)
	cargo test $(MCP)

clippy:
	cargo clippy --workspace --all-targets -- -D warnings
	cargo clippy $(ADK_PKGS) --all-targets -- -D warnings
	cargo clippy $(A2A_PKGS) --all-targets -- -D warnings
	cargo clippy $(ADK_A2A_PKGS) --all-targets -- -D warnings
	cargo clippy $(MCP) --all-targets -- -D warnings

env:
	@test -f .env || { \
		sed "s|^PLATFORM_JWT_SECRET=.*|PLATFORM_JWT_SECRET=$$(openssl rand -hex 32)|" .env.example > .env; \
		echo "created .env with a generated PLATFORM_JWT_SECRET"; }
	@grep -q '^LITELLM_MASTER_KEY=.' .env || { \
		{ grep -v '^LITELLM_MASTER_KEY=' .env; echo "LITELLM_MASTER_KEY=sk-$$(openssl rand -hex 24)"; } > .env.tmp && mv .env.tmp .env; \
		echo "generated LITELLM_MASTER_KEY in .env"; }

up: env
	$(if $(FEATURES),CARGO_FEATURES=$(FEATURES) )$(COMPOSE) up --build -d

prebuilt:
	cargo build --release -p docs-assistant $(if $(FEATURES),--features $(FEATURES))
	cargo build --release --manifest-path mcp/Cargo.toml -p docs-mcp -p tickets-mcp
	cp target/release/docs-assistant mcp/target/release/docs-mcp mcp/target/release/tickets-mcp deploy/prebuilt/

up-prebuilt: env prebuilt
	$(COMPOSE) -f docker-compose.yml -f docker-compose.prebuilt.yml up --build -d

down:
	$(COMPOSE) down

logs:
	$(COMPOSE) logs -f

up-minimal: env
	$(if $(FEATURES),CARGO_FEATURES=$(FEATURES) )$(COMPOSE) --env-file .env -f examples/minimal/docker-compose.yml up --build -d

up-mesh: env
	$(COMPOSE) --env-file .env -f examples/a2a-mesh/docker-compose.yml up --build -d

up-llm-gateway: env
	$(GATEWAY_COMPOSE) up -d

down-llm-gateway:
	$(GATEWAY_COMPOSE) down

# PREBUILT=1 packages host-built binaries like `up-prebuilt`.
up-with-gateway: env up-llm-gateway $(if $(PREBUILT),prebuilt)
	$(if $(FEATURES),CARGO_FEATURES=$(FEATURES) )$(COMPOSE) --env-file .env -f docker-compose.yml $(if $(PREBUILT),-f docker-compose.prebuilt.yml) -f deploy/llm-gateway/docker-compose.use-gateway.yml up --build -d

eval-mesh:
	cargo run -p agentic-eval -- --gateway http://localhost:8082 --golden examples/a2a-mesh/mesh-gateway/eval/golden.json --config examples/a2a-mesh/mesh-gateway/eval/eval.yaml

run-mesh-local: env
	./deploy/run-mesh-local.sh

eval:
	cargo run -p agentic-eval -- --gateway http://localhost:8080 --golden examples/docs-assistant/eval/golden.json --config examples/docs-assistant/eval/eval.yaml

run-local: env
	./deploy/run-local.sh
