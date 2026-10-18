mod corpus;

use agentic_mcp::{McpServerConfig, run};
use fastmcp_rust::prelude::*;
use serde_json::json;

#[tool(
    description = "Search the Nimbus Queue documentation. Returns a JSON list of {id, title, snippet, score}."
)]
async fn search_docs(ctx: &McpContext, query: String, limit: Option<u32>) -> McpResult<String> {
    ctx.checkpoint()?;
    let hits = corpus::search(corpus::CORPUS, &query, limit.unwrap_or(5) as usize);
    serde_json::to_string(&hits).map_err(|e| McpError::internal_error(e.to_string()))
}

#[tool(description = "Fetch one documentation page by id. Returns {id, title, content}.")]
async fn get_doc(ctx: &McpContext, id: String) -> McpResult<String> {
    ctx.checkpoint()?;
    let doc =
        corpus::find(&id).ok_or_else(|| McpError::tool_error(format!("unknown doc id: {id}")))?;
    Ok(json!({"id": doc.id, "title": doc.title, "content": doc.content}).to_string())
}

fn main() {
    let result = McpServerConfig::from_env("0.0.0.0:3001")
        .and_then(|config| run(config, |b| b.tool(SearchDocs).tool(GetDoc)));
    if let Err(e) = result {
        eprintln!("docs-mcp: {e}");
        std::process::exit(1);
    }
}
