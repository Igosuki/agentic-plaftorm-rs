use std::collections::HashSet;

use agentic_platform::mcp::CallOutcome;
use agentic_platform::prelude::*;
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
pub enum TicketAction {
    Listed,
    Viewed,
    Created,
    Denied,
    None,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct TicketRef {
    pub id: String,
    pub title: String,
    pub status: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct TicketsOutput {
    pub summary: String,
    pub action: TicketAction,
    pub tickets: Vec<TicketRef>,
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

impl AgentOutput for TicketsOutput {
    fn validate(&self, evidence: &Evidence) -> Result<(), Violation> {
        let returned = returned_ids(evidence, "tickets");
        if let Some(fake) = self.tickets.iter().find(|t| !returned.contains(&t.id)) {
            return Err(Violation::Hallucination(format!(
                "ticket `{}` was not returned by any tickets tool call in this run",
                fake.id
            )));
        }
        let creates = evidence.calls_to("tickets", "create_ticket");
        let created = creates.iter().any(|c| c.outcome == CallOutcome::Ok);
        let denied = creates.iter().any(|c| c.outcome == CallOutcome::Denied);
        if denied && self.action != TicketAction::Denied {
            return Err(Violation::Hallucination(
                "create_ticket was denied by policy, so action must be `denied`".into(),
            ));
        }
        if self.action == TicketAction::Created && !created {
            return Err(Violation::Hallucination(
                "action is `created` but no create_ticket call succeeded".into(),
            ));
        }
        if self.action == TicketAction::Denied && !denied {
            return Err(Violation::Hallucination(
                "action is `denied` but no tool call was denied; call create_ticket first".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(server: &str, tool: &str, outcome: CallOutcome, text: &str) -> ToolCallRecord {
        ToolCallRecord {
            server: server.into(),
            tool: tool.into(),
            args: Value::Null,
            outcome,
            result_text: text.into(),
            latency_ms: 1,
        }
    }

    fn docs_evidence() -> Evidence {
        Evidence::from_calls(vec![call(
            "docs",
            "search_docs",
            CallOutcome::Ok,
            r#"[{"id":"cfg-001","title":"Configuration Reference","snippet":"x","score":5}]"#,
        )])
    }

    fn docs(grounded: bool, ids: &[&str]) -> DocsOutput {
        DocsOutput {
            answer: "a".into(),
            grounded,
            sources: ids
                .iter()
                .map(|id| DocSource {
                    id: (*id).into(),
                    title: "t".into(),
                })
                .collect(),
        }
    }

    #[test]
    fn docs_accepts_retrieved_source() {
        assert_eq!(docs(true, &["cfg-001"]).validate(&docs_evidence()), Ok(()));
        assert_eq!(docs(false, &[]).validate(&docs_evidence()), Ok(()));
    }

    #[test]
    fn docs_rejects_fake_source_naming_it() {
        let err = docs(true, &["cfg-001", "zz-999"])
            .validate(&docs_evidence())
            .unwrap_err();
        assert!(matches!(&err, Violation::Hallucination(m) if m.contains("zz-999")));
    }

    #[test]
    fn docs_grounded_flag_must_match_sources() {
        assert!(matches!(
            docs(true, &[]).validate(&docs_evidence()),
            Err(Violation::Schema(_))
        ));
        assert!(matches!(
            docs(false, &["cfg-001"]).validate(&docs_evidence()),
            Err(Violation::Schema(_))
        ));
    }

    #[test]
    fn docs_ignores_failed_calls() {
        let ev = Evidence::from_calls(vec![call(
            "docs",
            "get_doc",
            CallOutcome::Error,
            r#"{"id":"cfg-001"}"#,
        )]);
        assert!(docs(true, &["cfg-001"]).validate(&ev).is_err());
    }

    fn tickets(action: TicketAction, ids: &[&str]) -> TicketsOutput {
        TicketsOutput {
            summary: "s".into(),
            action,
            tickets: ids
                .iter()
                .map(|id| TicketRef {
                    id: (*id).into(),
                    title: "t".into(),
                    status: "open".into(),
                })
                .collect(),
        }
    }

    #[test]
    fn tickets_listed_ids_must_be_returned() {
        let ev = Evidence::from_calls(vec![call(
            "tickets",
            "list_tickets",
            CallOutcome::Ok,
            r#"[{"id":"TCK-1001"},{"id":"TCK-1002"}]"#,
        )]);
        assert_eq!(
            tickets(TicketAction::Listed, &["TCK-1001"]).validate(&ev),
            Ok(())
        );
        assert!(matches!(
            tickets(TicketAction::Listed, &["TCK-9999"]).validate(&ev),
            Err(Violation::Hallucination(m)) if m.contains("TCK-9999")
        ));
    }

    #[test]
    fn tickets_created_requires_successful_call() {
        let ok = Evidence::from_calls(vec![call(
            "tickets",
            "create_ticket",
            CallOutcome::Ok,
            r#"{"id":"TCK-1005"}"#,
        )]);
        assert_eq!(
            tickets(TicketAction::Created, &["TCK-1005"]).validate(&ok),
            Ok(())
        );
        assert!(matches!(
            tickets(TicketAction::Created, &[]).validate(&Evidence::new()),
            Err(Violation::Hallucination(_))
        ));
    }

    #[test]
    fn tickets_denied_create_forces_denied_action() {
        let ev = Evidence::from_calls(vec![call(
            "tickets",
            "create_ticket",
            CallOutcome::Denied,
            "policy_denied: nope",
        )]);
        assert_eq!(tickets(TicketAction::Denied, &[]).validate(&ev), Ok(()));
        assert!(tickets(TicketAction::Created, &[]).validate(&ev).is_err());
        assert!(tickets(TicketAction::None, &[]).validate(&ev).is_err());
        assert!(
            tickets(TicketAction::Denied, &[])
                .validate(&Evidence::new())
                .is_err()
        );
    }
}
