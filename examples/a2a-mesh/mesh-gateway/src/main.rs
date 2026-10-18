mod outputs;

use agentic_platform::prelude::*;
use outputs::{DocsOutput, MathOutput, OpsOutput};

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
            Specialist::<OpsOutput>::new("ops_agent")
                .description("Triages operational incidents (consumer lag, full disks, broker outages) and names the runbook")
                .engine("a2a"),
        )
        .specialist(
            Specialist::<MathOutput>::new("math_agent")
                .description("Does arithmetic through a remote calculator agent")
                .engine("a2a"),
        )
        .build()
        .await?
        .serve()
        .await
}
