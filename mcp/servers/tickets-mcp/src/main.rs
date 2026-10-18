mod store;

use agentic_mcp::{McpServerConfig, run};
use fastmcp_rust::prelude::*;

fn to_json<T: serde::Serialize>(v: &T) -> McpResult<String> {
    serde_json::to_string(v).map_err(|e| McpError::internal_error(e.to_string()))
}

#[tool(
    description = "List tickets, optionally filtered by status (open, in_progress, closed). Returns a JSON list of tickets."
)]
async fn list_tickets(ctx: &McpContext, status: Option<String>) -> McpResult<String> {
    ctx.checkpoint()?;
    if let Some(s) = &status {
        store::Status::parse(s).map_err(McpError::invalid_params)?;
    }
    to_json(&store::list(status.as_deref()))
}

#[tool(description = "Get one ticket by id, e.g. TCK-1001.")]
async fn get_ticket(ctx: &McpContext, id: String) -> McpResult<String> {
    ctx.checkpoint()?;
    let ticket =
        store::get(&id).ok_or_else(|| McpError::tool_error(format!("unknown ticket id: {id}")))?;
    to_json(&ticket)
}

#[tool(
    description = "Create a ticket (priority: low, medium, high). The reporter is the authenticated caller. Returns the created ticket."
)]
async fn create_ticket(
    ctx: &McpContext,
    title: String,
    description: String,
    priority: String,
) -> McpResult<String> {
    ctx.checkpoint()?;
    let priority = store::Priority::parse(&priority).map_err(McpError::invalid_params)?;
    let reporter = ctx
        .auth()
        .and_then(|a| a.subject)
        .unwrap_or_else(|| "unknown".to_owned());
    to_json(&store::create(title, description, priority, reporter))
}

fn main() {
    let result = McpServerConfig::from_env("0.0.0.0:3002").and_then(|config| {
        run(config, |b| {
            b.tool(ListTickets).tool(GetTicket).tool(CreateTicket)
        })
    });
    if let Err(e) = result {
        eprintln!("tickets-mcp: {e}");
        std::process::exit(1);
    }
}
