use std::sync::Arc;

use adk_core::{AdkError, Tool, ToolContext};
use async_trait::async_trait;
use serde_json::{Value, json};

use crate::runbooks::{self, RUNBOOKS};

pub struct ListRunbooks;

#[async_trait]
impl Tool for ListRunbooks {
    fn name(&self) -> &str {
        "list_runbooks"
    }
    fn description(&self) -> &str {
        "List available Nimbus Queue runbooks (id, title, severity, symptoms)."
    }
    fn parameters_schema(&self) -> Option<Value> {
        Some(json!({"type": "object", "properties": {}}))
    }
    async fn execute(&self, _ctx: Arc<dyn ToolContext>, _args: Value) -> adk_core::Result<Value> {
        Ok(list())
    }
}

pub struct GetRunbook;

#[async_trait]
impl Tool for GetRunbook {
    fn name(&self) -> &str {
        "get_runbook"
    }
    fn description(&self) -> &str {
        "Fetch one runbook with its steps and thresholds by id."
    }
    fn parameters_schema(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {"id": {"type": "string", "description": "Runbook id from list_runbooks"}},
            "required": ["id"]
        }))
    }
    async fn execute(&self, _ctx: Arc<dyn ToolContext>, args: Value) -> adk_core::Result<Value> {
        get(&args).map_err(AdkError::tool)
    }
}

pub fn list() -> Value {
    let items: Vec<Value> = RUNBOOKS
        .iter()
        .map(|r| json!({"id": r.id, "title": r.title, "severity": r.severity, "symptoms": r.symptoms}))
        .collect();
    json!({"runbooks": items})
}

pub fn get(args: &Value) -> Result<Value, String> {
    let id = args
        .get("id")
        .and_then(Value::as_str)
        .ok_or("argument `id` (string) is required")?;
    let rb = runbooks::find(id)
        .ok_or_else(|| format!("unknown runbook id `{id}`; call list_runbooks"))?;
    serde_json::to_value(rb).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_has_all_ids() {
        let v = list();
        assert_eq!(v["runbooks"].as_array().unwrap().len(), RUNBOOKS.len());
        assert!(v.to_string().contains("rb-consumer-lag"));
    }

    #[test]
    fn get_returns_steps() {
        let v = get(&json!({"id": "rb-dlq-growth"})).unwrap();
        assert_eq!(v["id"], "rb-dlq-growth");
        assert!(v["steps"].as_array().unwrap().len() >= 3);
    }

    #[test]
    fn get_rejects_bad_input() {
        assert!(get(&json!({})).is_err());
        assert!(get(&json!({"id": "x"})).unwrap_err().contains("unknown"));
    }
}
