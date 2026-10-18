use std::collections::HashSet;

use agentic_platform::{mcp::CallOutcome, prelude::*};
use serde_json::Value;

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct DocSource {
    pub id: String,
    pub title: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct DocsOutput {
    pub answer: String,
    pub grounded: bool,
    pub sources: Vec<DocSource>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Low,
    Medium,
    High,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct OpsOutput {
    pub summary: String,
    pub runbook_id: Option<String>,
    pub severity: Severity,
    pub steps: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct MathOutput {
    pub answer: f64,
    pub explanation: String,
}

/// `^rb-[a-z0-9-]+$`
fn is_runbook_id(id: &str) -> bool {
    id.strip_prefix("rb-").is_some_and(|rest| {
        !rest.is_empty()
            && rest
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    })
}

impl AgentOutput for OpsOutput {
    fn validate(&self, _evidence: &Evidence) -> Result<(), Violation> {
        if self.steps.is_empty() {
            return Err(Violation::Schema("steps must not be empty".into()));
        }
        match &self.runbook_id {
            Some(id) if !is_runbook_id(id) => Err(Violation::Schema(format!(
                "runbook_id `{id}` must match ^rb-[a-z0-9-]+$ or be null"
            ))),
            _ => Ok(()),
        }
    }
}

impl AgentOutput for MathOutput {
    fn validate(&self, _evidence: &Evidence) -> Result<(), Violation> {
        if self.answer.is_finite() {
            Ok(())
        } else {
            Err(Violation::Schema("answer must be a finite number".into()))
        }
    }
}

/// `id` fields of the JSON objects returned (as an object or an array of objects) by successful calls to `server`.
fn returned_ids(evidence: &Evidence, server: &str) -> HashSet<String> {
    let mut ids = HashSet::new();
    for call in evidence.calls() {
        if call.server != server || call.outcome != CallOutcome::Ok {
            continue;
        }
        match serde_json::from_str::<Value>(&call.result_text) {
            Ok(Value::Array(items)) => ids.extend(items.iter().filter_map(id_of)),
            Ok(item) => ids.extend(id_of(&item)),
            Err(_) => {}
        }
    }
    ids
}

fn id_of(v: &Value) -> Option<String> {
    v.get("id")?.as_str().map(str::to_owned)
}

impl AgentOutput for DocsOutput {
    fn validate(&self, evidence: &Evidence) -> Result<(), Violation> {
        match (self.grounded, self.sources.is_empty()) {
            (true, true) => {
                return Err(Violation::Schema(
                    "grounded is true but sources is empty; cite the docs you used or set grounded=false".into(),
                ));
            }
            (false, false) => {
                return Err(Violation::Schema(
                    "grounded is false but sources is not empty; use an empty sources list".into(),
                ));
            }
            _ => {}
        }
        let retrieved = returned_ids(evidence, "docs");
        match self.sources.iter().find(|s| !retrieved.contains(&s.id)) {
            Some(fake) => Err(Violation::Hallucination(format!(
                "source `{}` was not returned by any docs tool call in this run",
                fake.id
            ))),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ops(runbook_id: Option<&str>, steps: &[&str]) -> OpsOutput {
        OpsOutput {
            summary: "s".into(),
            runbook_id: runbook_id.map(str::to_owned),
            severity: Severity::High,
            steps: steps.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    #[test]
    fn runbook_ids_follow_the_pattern() {
        for ok in ["rb-consumer-lag", "rb-1", "rb-a-b-2"] {
            assert!(is_runbook_id(ok), "{ok}");
        }
        for bad in ["rb-", "RB-x", "rb-X", "runbook-1", "rb-a_b", "rb-a b", ""] {
            assert!(!is_runbook_id(bad), "{bad}");
        }
    }

    #[test]
    fn ops_output_needs_steps_and_a_well_formed_runbook() {
        let e = Evidence::new();
        assert_eq!(
            ops(Some("rb-disk-full"), &["free space"]).validate(&e),
            Ok(())
        );
        assert_eq!(ops(None, &["page oncall"]).validate(&e), Ok(()));
        assert!(matches!(
            ops(None, &[]).validate(&e),
            Err(Violation::Schema(_))
        ));
        assert!(matches!(
            ops(Some("disk-full"), &["x"]).validate(&e),
            Err(Violation::Schema(_))
        ));
    }

    #[test]
    fn math_answer_must_be_finite() {
        let m = |answer| MathOutput {
            answer,
            explanation: String::new(),
        };
        assert_eq!(m(42.0).validate(&Evidence::new()), Ok(()));
        assert!(m(f64::NAN).validate(&Evidence::new()).is_err());
    }
}
