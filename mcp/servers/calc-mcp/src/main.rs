use agentic_mcp::{McpServerConfig, run};
use fastmcp_rust::prelude::*;

#[tool(description = "Add two numbers")]
async fn add(ctx: &McpContext, a: f64, b: f64) -> McpResult<String> {
    ctx.checkpoint()?;
    Ok((a + b).to_string())
}

#[tool(description = "Multiply two numbers")]
async fn multiply(ctx: &McpContext, a: f64, b: f64) -> McpResult<String> {
    ctx.checkpoint()?;
    Ok((a * b).to_string())
}

fn main() {
    let result = McpServerConfig::from_env("0.0.0.0:3003")
        .and_then(|config| run(config, |b| b.tool(Add).tool(Multiply)));
    if let Err(e) = result {
        eprintln!("calc-mcp: {e}");
        std::process::exit(1);
    }
}
