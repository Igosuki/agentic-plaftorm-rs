use std::time::Duration;

use serde_json::{Value, json};

pub enum RunReply {
    Answered(Value),
    Unclassified(Value),
    AgentFailed(Value),
}

pub struct Gateway {
    http: reqwest::Client,
    base: String,
}

fn error_code(body: &Value) -> Option<&str> {
    body["error"]["code"].as_str()
}

impl Gateway {
    pub fn new(base: &str, timeout: Duration) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self {
            http,
            base: base.trim_end_matches('/').to_owned(),
        })
    }

    async fn send(
        &self,
        req: reqwest::RequestBuilder,
        what: &str,
    ) -> Result<(reqwest::StatusCode, Value), String> {
        let resp = req.send().await.map_err(|e| format!("{what}: {e}"))?;
        let status = resp.status();
        let text = resp.text().await.map_err(|e| format!("{what}: {e}"))?;
        let body = serde_json::from_str(&text).unwrap_or(Value::String(text));
        Ok((status, body))
    }

    pub async fn token(&self, api_key: &str) -> Result<String, String> {
        let req = self
            .http
            .post(format!("{}/auth/token", self.base))
            .json(&json!({"api_key": api_key}));
        let (status, body) = self.send(req, "POST /auth/token").await?;
        match (status.is_success(), body["access_token"].as_str()) {
            (true, Some(t)) => Ok(t.to_owned()),
            _ => Err(format!("POST /auth/token returned {status}: {body}")),
        }
    }

    pub async fn run(&self, token: &str, input: &str) -> Result<RunReply, String> {
        let req = self
            .http
            .post(format!("{}/v1/agents/run", self.base))
            .bearer_auth(token)
            .json(&json!({"input": input}));
        let (status, body) = self.send(req, "POST /v1/agents/run").await?;
        match (status.as_u16(), error_code(&body)) {
            (200, _) => Ok(RunReply::Answered(body)),
            (422, Some("unclassified")) => Ok(RunReply::Unclassified(body)),
            (502, Some("agent_failed")) => Ok(RunReply::AgentFailed(body)),
            _ => Err(format!("POST /v1/agents/run returned {status}: {body}")),
        }
    }

    pub async fn live_version(&self, admin_token: &str, prompt_id: &str) -> Result<String, String> {
        let req = self
            .http
            .get(format!("{}/admin/prompts/{prompt_id}", self.base))
            .bearer_auth(admin_token);
        let (status, body) = self.send(req, "GET /admin/prompts").await?;
        match (status.is_success(), body["live"].as_str()) {
            (true, Some(v)) => Ok(v.to_owned()),
            _ => Err(format!(
                "GET /admin/prompts/{prompt_id} returned {status}: {body}"
            )),
        }
    }

    pub async fn set_live(
        &self,
        admin_token: &str,
        prompt_id: &str,
        version: &str,
    ) -> Result<(), String> {
        let req = self
            .http
            .post(format!("{}/admin/prompts/{prompt_id}/live", self.base))
            .bearer_auth(admin_token)
            .json(&json!({"version": version, "record_history": false}));
        let (status, body) = self.send(req, "POST /admin/prompts/live").await?;
        if status.is_success() {
            Ok(())
        } else {
            Err(format!(
                "POST /admin/prompts/{prompt_id}/live returned {status}: {body}"
            ))
        }
    }
}
