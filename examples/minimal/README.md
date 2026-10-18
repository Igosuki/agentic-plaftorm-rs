# Minimal example

One specialist (`math_agent`), one MCP server (`calc`), one user. Copy this directory to start a new agentic project.

## Checklist

1. Copy `examples/minimal` to `examples/<name>`; rename the package in `Cargo.toml` and add it to the workspace `members` in the root `Cargo.toml`.
2. In `src/main.rs`, define the output type (`Serialize + Deserialize + JsonSchema`) and implement `AgentOutput::validate` to reject answers that the run's `Evidence` does not support.
3. Register the specialist with `Specialist::<T>::new("<agent>").description(..).mcp_server("<server>")`.
4. Write `config/prompts/<agent>.yaml`: tool names, the procedure, and a one-line JSON example. The framework appends the output schema.
5. Update `config/intents.yaml` with the keywords and phrases that route to the agent.
6. Update `config/policies.yaml` with the roles allowed to call each `<server>.<tool>`; `config/users.yaml` maps API keys to roles.
7. Build the MCP server from the `mcp/servers/calc-mcp` template and point `mcp_servers` in `config/platform.yaml` at it. The policy file is shared with the server through `POLICY_FILE`.
8. Add the gateway and MCP server to a compose file (see `docker-compose.yml`; `./config` is mounted at `/app/config`).
9. Write `eval/golden.json` and `eval/eval.yaml` and gate changes with `agentic-eval`.

## Run

```sh
make up-minimal
cargo run -p agentic-eval -- --gateway http://localhost:8081 \
  --golden examples/minimal/eval/golden.json --config examples/minimal/eval/eval.yaml
```
