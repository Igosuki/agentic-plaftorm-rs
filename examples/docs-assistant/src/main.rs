mod outputs;

use agentic_platform::prelude::*;
use outputs::{DocsOutput, TicketsOutput};

#[tokio::main]
async fn main() -> agentic_platform::Result<()> {
    Platform::from_config(config_path_from_env_or_args())?
        .specialist(
            Specialist::<DocsOutput>::new("docs_agent")
                .description("Answers Nimbus Queue product documentation questions, grounded in retrieved docs")
                .mcp_server("docs")
                .max_turns(8),
        )
        .specialist(
            Specialist::<TicketsOutput>::new("tickets_agent")
                .description("Lists, views and creates support tickets")
                .mcp_server("tickets")
                .max_turns(8),
        )
        .build()
        .await?
        .serve()
        .await
}
