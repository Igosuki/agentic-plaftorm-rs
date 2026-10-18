use agentic_platform::{mcp::CallOutcome, prelude::*};

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct MathOutput {
    answer: f64,
    explanation: String,
}

impl AgentOutput for MathOutput {
    fn validate(&self, evidence: &Evidence) -> Result<(), Violation> {
        let grounded = evidence
            .calls_to("calc", "add")
            .into_iter()
            .chain(evidence.calls_to("calc", "multiply"))
            .filter(|c| c.outcome == CallOutcome::Ok)
            .filter_map(|c| c.result_text.trim().trim_matches('"').parse::<f64>().ok())
            .any(|n| (n - self.answer).abs() < 1e-9);
        if grounded {
            Ok(())
        } else {
            Err(Violation::Hallucination(format!(
                "answer {} is not a result of any successful calc tool call",
                self.answer
            )))
        }
    }
}

#[tokio::main]
async fn main() -> agentic_platform::Result<()> {
    Platform::from_config(config_path_from_env_or_args())?
        .specialist(
            Specialist::<MathOutput>::new("math_agent")
                .description("Does arithmetic with the calc tools")
                .mcp_server("calc"),
        )
        .build()
        .await?
        .serve()
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evidence(outcome: CallOutcome, text: &str) -> Evidence {
        Evidence::from_calls(vec![ToolCallRecord {
            server: "calc".into(),
            tool: "add".into(),
            args: serde_json::Value::Null,
            outcome,
            result_text: text.into(),
            latency_ms: 1,
        }])
    }

    fn out(answer: f64) -> MathOutput {
        MathOutput {
            answer,
            explanation: String::new(),
        }
    }

    #[test]
    fn accepts_tool_result() {
        assert_eq!(out(5.0).validate(&evidence(CallOutcome::Ok, "5")), Ok(()));
    }

    #[test]
    fn rejects_unsupported_answer_and_failed_calls() {
        assert!(matches!(
            out(6.0).validate(&evidence(CallOutcome::Ok, "5")),
            Err(Violation::Hallucination(_))
        ));
        assert!(
            out(5.0)
                .validate(&evidence(CallOutcome::Error, "5"))
                .is_err()
        );
        assert!(out(5.0).validate(&Evidence::new()).is_err());
    }
}
