use std::time::Duration;

use agentic_core::auth::Claims;
use schemars::JsonSchema;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

pub use crate::engine::TokenUsage;
use crate::{
    config::GuardConfig,
    engine::{AgentEngine, Attribution, ToolSpec, TurnError, TurnRequest},
    mcp::Evidence,
};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Violation {
    #[error("schema violation: {0}")]
    Schema(String),
    #[error("hallucination: {0}")]
    Hallucination(String),
}

pub trait AgentOutput: Serialize + DeserializeOwned + JsonSchema + Send + Sync + 'static {
    /// Grounding checks run after schema validation; `Violation::Hallucination` triggers a re-prompt.
    fn validate(&self, _evidence: &Evidence) -> Result<(), Violation> {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptOutcome {
    Ok,
    SchemaViolation,
    Hallucination,
    MaxTurns,
    InfraError,
    Rejected,
}

impl AttemptOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::SchemaViolation => "schema_violation",
            Self::Hallucination => "hallucination",
            Self::MaxTurns => "max_turns",
            Self::InfraError => "infra_error",
            Self::Rejected => "rejected",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct AttemptRecord {
    pub attempt: u32,
    pub outcome: AttemptOutcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug)]
pub struct GuardResult<T> {
    pub output: Option<T>,
    pub attempts: Vec<AttemptRecord>,
    pub usage: TokenUsage,
}

pub struct GuardRequest<'a> {
    pub engine: &'a dyn AgentEngine,
    pub model_spec: &'a str,
    pub preamble: &'a str,
    pub temperature: Option<f64>,
    pub max_turns: usize,
    pub tools: Vec<ToolSpec>,
    pub input: &'a str,
    pub evidence: &'a Evidence,
    pub config: &'a GuardConfig,
    pub app: &'a str,
    pub agent: &'a str,
    pub use_case: &'a str,
    pub prompt_version: &'a str,
    pub caller: Option<Claims>,
    pub request_id: &'a str,
}

pub fn output_schema<T: JsonSchema>() -> Value {
    serde_json::to_value(schemars::schema_for!(T)).unwrap_or(Value::Null)
}

fn schema_preamble(template: &str, schema: &Value) -> String {
    format!(
        "{template}\n\nRespond ONLY with a JSON object matching this JSON Schema (no prose, no markdown fences):\n{}",
        serde_json::to_string_pretty(schema).unwrap_or_default()
    )
}

/// First balanced `{...}` in `text` that parses as JSON; tolerates fences and surrounding prose.
pub fn extract_json(text: &str) -> Option<Value> {
    let bytes = text.as_bytes();
    let mut from = 0;
    while let Some(rel) = text[from..].find('{') {
        let start = from + rel;
        let (mut depth, mut in_str, mut esc) = (0usize, false, false);
        for (i, &b) in bytes.iter().enumerate().skip(start) {
            if in_str {
                match (esc, b) {
                    (true, _) => esc = false,
                    (false, b'\\') => esc = true,
                    (false, b'"') => in_str = false,
                    _ => {}
                }
                continue;
            }
            match b {
                b'"' => in_str = true,
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        if let Ok(v @ Value::Object(_)) = serde_json::from_str(&text[start..=i]) {
                            return Some(v);
                        }
                        break;
                    }
                }
                _ => {}
            }
        }
        from = start + 1;
    }
    None
}

/// Parse, validate against `schema`, deserialize, then run `T::validate`.
pub fn check_output<T: AgentOutput>(
    text: &str,
    validator: &jsonschema::Validator,
    evidence: &Evidence,
) -> Result<T, (AttemptOutcome, String)> {
    let schema_err = |m: String| (AttemptOutcome::SchemaViolation, m);
    let value =
        extract_json(text).ok_or_else(|| schema_err("response contained no JSON object".into()))?;
    let errors: Vec<String> = validator
        .iter_errors(&value)
        .map(|e| format!("{} (at `{}`)", e, e.instance_path()))
        .collect();
    if !errors.is_empty() {
        return Err(schema_err(errors.join("; ")));
    }
    let out: T = serde_json::from_value(value).map_err(|e| schema_err(e.to_string()))?;
    out.validate(evidence).map_err(|v| match v {
        Violation::Schema(m) => (AttemptOutcome::SchemaViolation, m),
        Violation::Hallucination(m) => (AttemptOutcome::Hallucination, m),
    })?;
    Ok(out)
}

fn feedback(outcome: AttemptOutcome, detail: &str, evidence: &Evidence) -> String {
    match outcome {
        AttemptOutcome::Hallucination => format!(
            "Your previous answer failed grounding checks: {detail}\nUse only facts present in tool results. Tool results so far:\n{}\nRespond again with ONLY the corrected JSON object.",
            evidence.summary(500)
        ),
        _ => format!(
            "Your previous response was rejected: {detail}\nRespond again with ONLY a JSON object that matches the schema."
        ),
    }
}

fn max_turns_nudge(input: &str, evidence: &Evidence) -> String {
    format!(
        "{input}\n\nYou ran out of tool-call turns before answering. Stop calling tools and answer now with ONLY the JSON object. Tool results gathered so far:\n{}",
        evidence.summary(500)
    )
}

pub(crate) fn backoff(base_ms: u64, infra_failures: u32) -> Duration {
    let ms = base_ms.saturating_mul(1u64 << (infra_failures - 1).min(6));
    Duration::from_millis(ms.min(10_000))
}

pub async fn run<T: AgentOutput>(req: GuardRequest<'_>) -> GuardResult<T> {
    let schema = output_schema::<T>();
    let preamble = schema_preamble(req.preamble, &schema);
    let mut result = GuardResult {
        output: None,
        attempts: Vec::new(),
        usage: TokenUsage::default(),
    };
    let validator = match jsonschema::validator_for(&schema) {
        Ok(v) => v,
        Err(e) => {
            result.attempts.push(AttemptRecord {
                attempt: 1,
                outcome: AttemptOutcome::InfraError,
                detail: Some(format!("output schema does not compile: {e}")),
            });
            crate::telemetry::record_attempt(
                req.app,
                req.agent,
                req.prompt_version,
                AttemptOutcome::InfraError,
            );
            return result;
        }
    };

    let attribution = Attribution {
        user: req
            .caller
            .as_ref()
            .map(|c| c.sub.clone())
            .unwrap_or_default(),
        team: req
            .caller
            .as_ref()
            .map(|c| c.team.clone())
            .unwrap_or_default(),
        application: req.app.to_owned(),
        use_case: req.use_case.to_owned(),
        agent: req.agent.to_owned(),
        prompt_version: req.prompt_version.to_owned(),
        request_id: req.request_id.to_owned(),
    };
    let turn = TurnRequest {
        model_spec: req.model_spec.to_owned(),
        preamble,
        temperature: req.temperature,
        max_turns: req.max_turns,
        tools: req.tools,
        caller: req.caller,
        request_id: req.request_id.to_owned(),
        attribution,
    };
    let mut conversation = req.engine.conversation();
    let mut next_message = req.input.to_owned();
    let mut infra_failures = 0u32;

    for attempt in 1..=req.config.max_attempts.max(1) {
        let (outcome, detail, used) = match req
            .engine
            .chat(&turn, &mut conversation, next_message.clone())
            .await
        {
            Ok(resp) => {
                let (outcome, detail) =
                    match check_output::<T>(&resp.text, &validator, req.evidence) {
                        Ok(out) => {
                            result.output = Some(out);
                            (AttemptOutcome::Ok, None)
                        }
                        Err((outcome, detail)) => {
                            next_message = feedback(outcome, &detail, req.evidence);
                            (outcome, Some(detail))
                        }
                    };
                (outcome, detail, resp.usage)
            }
            Err(e @ TurnError::MaxTurns { .. }) => {
                next_message = max_turns_nudge(req.input, req.evidence);
                (AttemptOutcome::MaxTurns, Some(e.to_string()), e.usage())
            }
            Err(e @ TurnError::Rejected { .. }) => {
                (AttemptOutcome::Rejected, Some(e.to_string()), e.usage())
            }
            Err(e) => (AttemptOutcome::InfraError, Some(e.to_string()), e.usage()),
        };
        result.usage.input_tokens += used.input_tokens;
        result.usage.output_tokens += used.output_tokens;

        crate::telemetry::record_attempt(req.app, req.agent, req.prompt_version, outcome);
        if let Some(d) = &detail {
            tracing::warn!(
                agent = req.agent,
                prompt_version = req.prompt_version,
                attempt,
                outcome = outcome.as_str(),
                detail = %d,
                "guard attempt failed"
            );
        }
        result.attempts.push(AttemptRecord {
            attempt,
            outcome,
            detail,
        });
        match outcome {
            AttemptOutcome::Ok => break,
            AttemptOutcome::Rejected => break,
            AttemptOutcome::InfraError => infra_failures += 1,
            _ => infra_failures = 0,
        }
        if attempt >= req.config.max_attempts {
            break;
        }
        if result.usage.total() >= req.config.max_total_tokens {
            tracing::warn!(
                agent = req.agent,
                used = result.usage.total(),
                "guard token budget exhausted"
            );
            break;
        }
        if outcome == AttemptOutcome::InfraError {
            tokio::time::sleep(backoff(req.config.backoff_base_ms, infra_failures)).await;
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;
    use crate::mcp::{CallOutcome, ToolCallRecord};

    #[derive(Debug, Serialize, Deserialize, JsonSchema, PartialEq)]
    struct Out {
        answer: String,
        id: u32,
    }

    impl AgentOutput for Out {
        fn validate(&self, evidence: &Evidence) -> Result<(), Violation> {
            if evidence.mentions(&format!("DOC-{}", self.id)) {
                Ok(())
            } else {
                Err(Violation::Hallucination(format!(
                    "DOC-{} not in tool results",
                    self.id
                )))
            }
        }
    }

    fn validator() -> jsonschema::Validator {
        jsonschema::validator_for(&output_schema::<Out>()).unwrap()
    }

    fn evidence() -> Evidence {
        Evidence::from_calls(vec![ToolCallRecord {
            server: "docs".into(),
            tool: "get".into(),
            args: Value::Null,
            outcome: CallOutcome::Ok,
            result_text: "DOC-7".into(),
            latency_ms: 0,
        }])
    }

    #[test]
    fn extracts_plain_fenced_and_prose_wrapped() {
        let want = serde_json::json!({"a": 1});
        assert_eq!(extract_json(r#"{"a":1}"#), Some(want.clone()));
        assert_eq!(extract_json("```json\n{\"a\": 1}\n```"), Some(want.clone()));
        assert_eq!(
            extract_json("Sure! Here you go: {\"a\":1} hope it helps"),
            Some(want)
        );
    }

    #[test]
    fn extraction_handles_nesting_and_braces_in_strings() {
        let v = extract_json(r#"x {"a":{"b":"}{"},"c":[{"d":1}]} y {"z":1}"#).unwrap();
        assert_eq!(v["a"]["b"], "}{");
        assert_eq!(v["c"][0]["d"], 1);
    }

    #[test]
    fn extraction_skips_non_json_braces() {
        assert_eq!(extract_json("use {curly} then {\"a\":2}").unwrap()["a"], 2);
        assert!(extract_json("no json here").is_none());
        assert!(extract_json("{\"unterminated\": 1").is_none());
    }

    #[test]
    fn classifies_ok() {
        let out: Out = check_output(r#"{"answer":"x","id":7}"#, &validator(), &evidence()).unwrap();
        assert_eq!(
            out,
            Out {
                answer: "x".into(),
                id: 7
            }
        );
    }

    #[test]
    fn classifies_schema_violations() {
        let v = validator();
        let e = evidence();
        for text in [
            "plain prose",
            r#"{"answer":"x"}"#,
            r#"{"answer":1,"id":7}"#,
            r#"{"answer":"x","id":-1}"#,
        ] {
            let (o, _) = check_output::<Out>(text, &v, &e).unwrap_err();
            assert_eq!(o, AttemptOutcome::SchemaViolation, "{text}");
        }
    }

    #[test]
    fn classifies_hallucination() {
        let (o, d) =
            check_output::<Out>(r#"{"answer":"x","id":9}"#, &validator(), &evidence()).unwrap_err();
        assert_eq!(o, AttemptOutcome::Hallucination);
        assert!(d.contains("DOC-9"));
    }

    #[test]
    fn max_turns_nudge_keeps_question_and_evidence() {
        let n = max_turns_nudge("what is 2+3?", &evidence());
        assert!(n.starts_with("what is 2+3?"));
        assert!(n.contains("Stop calling tools"));
        assert!(n.contains("DOC-7"));
        assert_eq!(AttemptOutcome::MaxTurns.as_str(), "max_turns");
    }

    #[test]
    fn backoff_grows_and_caps() {
        assert_eq!(backoff(100, 1), Duration::from_millis(100));
        assert_eq!(backoff(100, 3), Duration::from_millis(400));
        assert_eq!(backoff(100, 30), Duration::from_millis(6400));
        assert_eq!(backoff(5000, 5), Duration::from_millis(10_000));
    }
}
