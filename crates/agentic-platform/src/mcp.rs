use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use agentic_core::{
    auth::{Claims, JwtKeys},
    mcp_wire::{self, DENIED_META_KEY, POLICY_DENIED_CODE, POLICY_DENIED_PREFIX},
    policy::Policy,
};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{config::ToolVisibility, engine::ToolSpec};

#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("transport: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("server rejected the bearer token (HTTP 401)")]
    Unauthorized,
    #[error("unexpected HTTP status {0}")]
    Status(StatusCode),
    #[error("protocol: {0}")]
    Protocol(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct McpToolInfo {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ToolOutcome {
    Ok {
        text: String,
        structured: Option<Value>,
    },
    Error {
        text: String,
    },
    Denied {
        reason: String,
    },
}

#[derive(Debug, Clone)]
pub struct McpClient {
    http: reqwest::Client,
    url: String,
    next_id: Arc<AtomicU64>,
}

impl McpClient {
    pub fn new(url: impl Into<String>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("static reqwest client configuration");
        Self {
            http,
            url: url.into(),
            next_id: Arc::new(AtomicU64::new(1)),
        }
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    async fn rpc(
        &self,
        token: &str,
        method: &str,
        name: Option<&str>,
        mut params: Value,
    ) -> Result<Value, McpError> {
        params["_meta"] = mcp_wire::request_meta();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let body = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let mut req = self
            .http
            .post(&self.url)
            .header(
                mcp_wire::HEADER_PROTOCOL_VERSION,
                mcp_wire::PROTOCOL_VERSION,
            )
            .header(mcp_wire::HEADER_METHOD, method)
            .header(reqwest::header::ACCEPT, mcp_wire::ACCEPT)
            .bearer_auth(token)
            .json(&body);
        if let Some(n) = name {
            req = req.header(mcp_wire::HEADER_NAME, n);
        }
        let resp = req.send().await?;
        match resp.status() {
            StatusCode::UNAUTHORIZED => return Err(McpError::Unauthorized),
            StatusCode::FORBIDDEN => {
                let text = resp.text().await.unwrap_or_default();
                return Ok(
                    json!({"error": {"code": POLICY_DENIED_CODE, "message": format!("{POLICY_DENIED_PREFIX} HTTP 403 {text}")}}),
                );
            }
            s if !s.is_success() => {
                return Err(McpError::Status(s));
            }
            _ => {}
        }
        let is_sse = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("text/event-stream"));
        let text = resp.text().await?;
        parse_response(&text, is_sse, id)
    }

    pub async fn list_tools(&self, token: &str) -> Result<Vec<McpToolInfo>, McpError> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = cursor
                .as_ref()
                .map_or_else(|| json!({}), |c| json!({"cursor": c}));
            let reply = self
                .rpc(token, mcp_wire::METHOD_TOOLS_LIST, None, params)
                .await?;
            let result = rpc_result(reply)?;
            for t in result["tools"].as_array().into_iter().flatten() {
                let name = t["name"]
                    .as_str()
                    .ok_or_else(|| McpError::Protocol("tool without a name".into()))?;
                tools.push(McpToolInfo {
                    name: name.to_owned(),
                    description: t["description"].as_str().unwrap_or_default().to_owned(),
                    input_schema: t
                        .get("inputSchema")
                        .cloned()
                        .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
                });
            }
            match result["nextCursor"].as_str() {
                Some(c) if !c.is_empty() => cursor = Some(c.to_owned()),
                _ => return Ok(tools),
            }
        }
    }

    pub async fn call_tool(
        &self,
        token: &str,
        name: &str,
        arguments: Value,
    ) -> Result<ToolOutcome, McpError> {
        let arguments = if arguments.is_null() {
            json!({})
        } else {
            arguments
        };
        let reply = self
            .rpc(
                token,
                mcp_wire::METHOD_TOOLS_CALL,
                Some(name),
                json!({"name": name, "arguments": arguments}),
            )
            .await?;
        if let Some(err) = reply.get("error") {
            let message = err["message"]
                .as_str()
                .unwrap_or("unknown error")
                .to_owned();
            let denied = err["code"].as_i64() == Some(POLICY_DENIED_CODE)
                || message.starts_with(POLICY_DENIED_PREFIX);
            return Ok(if denied {
                ToolOutcome::Denied { reason: message }
            } else {
                ToolOutcome::Error { text: message }
            });
        }
        let result = reply
            .get("result")
            .ok_or_else(|| McpError::Protocol("response has neither result nor error".into()))?;
        let text = content_text(result);
        let is_error = result["isError"].as_bool().unwrap_or(false);
        let flagged_denied = result["_meta"][DENIED_META_KEY].as_bool() == Some(true);
        Ok(
            if flagged_denied || (is_error && text.starts_with(POLICY_DENIED_PREFIX)) {
                ToolOutcome::Denied { reason: text }
            } else if is_error {
                ToolOutcome::Error { text }
            } else {
                ToolOutcome::Ok {
                    text,
                    structured: result.get("structuredContent").cloned(),
                }
            },
        )
    }
}

fn rpc_result(reply: Value) -> Result<Value, McpError> {
    if let Some(err) = reply.get("error") {
        return Err(McpError::Protocol(format!(
            "rpc error {}: {}",
            err["code"],
            err["message"].as_str().unwrap_or("")
        )));
    }
    reply
        .get("result")
        .cloned()
        .ok_or_else(|| McpError::Protocol("response has neither result nor error".into()))
}

fn content_text(result: &Value) -> String {
    let parts: Vec<&str> = result["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c["text"].as_str())
        .collect();
    if parts.is_empty() {
        result
            .get("structuredContent")
            .map(Value::to_string)
            .unwrap_or_default()
    } else {
        parts.join("\n")
    }
}

/// A reply answers `id` when ids match, or when it is an error with a null id (JSON-RPC's "id undeterminable").
fn is_reply_to(v: &Value, id: u64) -> bool {
    if v.get("result").is_none() && v.get("error").is_none() {
        return false;
    }
    match v.get("id") {
        Some(Value::Null) | None => v.get("error").is_some(),
        Some(got) => got.as_u64() == Some(id),
    }
}

fn parse_response(body: &str, is_sse: bool, id: u64) -> Result<Value, McpError> {
    if !is_sse && let Ok(v) = serde_json::from_str::<Value>(body) {
        return if is_reply_to(&v, id) {
            Ok(v)
        } else {
            Err(McpError::Protocol(format!(
                "response does not answer request id {id}"
            )))
        };
    }
    let mut data = String::new();
    let mut found: Option<Value> = None;
    let mut flush = |data: &mut String| {
        if !data.is_empty() {
            if let Ok(v) = serde_json::from_str::<Value>(data)
                && is_reply_to(&v, id)
            {
                found = Some(v);
            }
            data.clear();
        }
    };
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
        } else if line.trim().is_empty() {
            flush(&mut data);
        }
    }
    flush(&mut data);
    found.ok_or_else(|| McpError::Protocol(format!("no JSON-RPC response with id {id} in body")))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CallOutcome {
    Ok,
    Error,
    Denied,
}

impl CallOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "error",
            Self::Denied => "denied",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallRecord {
    pub server: String,
    pub tool: String,
    pub args: Value,
    pub outcome: CallOutcome,
    pub result_text: String,
    pub latency_ms: u64,
}

/// Tool calls made during one request, shared between the tool wrappers and `AgentOutput::validate`.
#[derive(Debug, Clone, Default)]
pub struct Evidence {
    calls: Arc<Mutex<Vec<ToolCallRecord>>>,
}

impl Evidence {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_calls(calls: Vec<ToolCallRecord>) -> Self {
        Self {
            calls: Arc::new(Mutex::new(calls)),
        }
    }

    pub fn record(&self, call: ToolCallRecord) {
        self.calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(call);
    }

    pub fn calls(&self) -> Vec<ToolCallRecord> {
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn calls_to(&self, server: &str, tool: &str) -> Vec<ToolCallRecord> {
        self.calls()
            .into_iter()
            .filter(|c| c.server == server && c.tool == tool)
            .collect()
    }

    /// Result text of successful calls only, newline-joined.
    pub fn results_text(&self) -> String {
        self.calls()
            .iter()
            .filter(|c| c.outcome == CallOutcome::Ok)
            .map(|c| c.result_text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Case-insensitive substring search over successful results.
    pub fn mentions(&self, needle: &str) -> bool {
        !needle.is_empty()
            && self
                .results_text()
                .to_lowercase()
                .contains(&needle.to_lowercase())
    }

    pub(crate) fn summary(&self, max_each: usize) -> String {
        let calls = self.calls();
        if calls.is_empty() {
            return "no tool calls were made".into();
        }
        calls
            .iter()
            .map(|c| {
                let mut text: String = c.result_text.chars().take(max_each).collect();
                if c.result_text.chars().count() > max_each {
                    text.push('…');
                }
                format!(
                    "{}.{}({}) [{}] -> {}",
                    c.server,
                    c.tool,
                    c.args,
                    c.outcome.as_str(),
                    text
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Policy ids are `<server>.<tool>` globs, so a tool name containing `.` or `*` would be ambiguous to match.
fn usable_tools(server: &str, tools: Vec<McpToolInfo>) -> Vec<McpToolInfo> {
    tools
        .into_iter()
        .filter(|t| {
            let bad = t.name.contains(['.', '*']);
            if bad {
                tracing::warn!(server, tool = %t.name, "skipping MCP tool whose name contains `.` or `*`");
            }
            !bad
        })
        .collect()
}

struct ServerTools {
    client: McpClient,
    tools: Vec<McpToolInfo>,
}

pub struct McpHub {
    servers: BTreeMap<String, ServerTools>,
    keys: JwtKeys,
    issuer: String,
    delegated_ttl_secs: i64,
    policy: Arc<Policy>,
    visibility: ToolVisibility,
}

/// Per-request identity and sinks for tool wrappers.
#[derive(Clone)]
pub struct ToolSession {
    pub claims: Claims,
    pub request_id: String,
    pub app: String,
    pub agent: String,
    pub evidence: Evidence,
}

impl McpHub {
    /// Fetches the tool catalog of each `(name, url)` with a service token; retries transport failures briefly so a stack can start in any order.
    pub async fn connect(
        servers: impl IntoIterator<Item = (String, String)>,
        keys: JwtKeys,
        issuer: String,
        delegated_ttl_secs: i64,
        policy: Arc<Policy>,
        visibility: ToolVisibility,
    ) -> Result<Self, McpError> {
        let mut connected = BTreeMap::new();
        for (name, url) in servers {
            let client = McpClient::new(url);
            let token = keys
                .issue_service_token(&issuer, 60)
                .map_err(|e| McpError::Protocol(e.to_string()))?;
            let mut attempt = 0;
            let tools = loop {
                attempt += 1;
                match client.list_tools(&token).await {
                    Ok(t) => break t,
                    Err(e @ (McpError::Transport(_) | McpError::Status(_))) if attempt < 5 => {
                        tracing::warn!(server = %name, attempt, error = %e, "mcp tools/list failed, retrying");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                    Err(e) => {
                        return Err(McpError::Protocol(format!(
                            "server `{name}` ({}): {e}",
                            client.url()
                        )));
                    }
                }
            };
            let tools = usable_tools(&name, tools);
            tracing::info!(server = %name, tools = tools.len(), "mcp server connected");
            connected.insert(name, ServerTools { client, tools });
        }
        Ok(Self {
            servers: connected,
            keys,
            issuer,
            delegated_ttl_secs,
            policy,
            visibility,
        })
    }

    pub fn tool_names(&self, server: &str) -> Vec<String> {
        self.servers
            .get(server)
            .map(|s| s.tools.iter().map(|t| t.name.clone()).collect())
            .unwrap_or_default()
    }

    /// Model-facing names are the bare tool name when unique across `servers`, `<server>__<tool>` otherwise.
    pub fn tools_for(&self, servers: &[String], session: &ToolSession) -> Vec<ToolSpec> {
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for s in servers {
            for t in self.servers.get(s).into_iter().flat_map(|s| &s.tools) {
                *counts.entry(t.name.as_str()).or_default() += 1;
            }
        }
        let mut out = Vec::new();
        for server in servers {
            let Some(st) = self.servers.get(server) else {
                continue;
            };
            for info in &st.tools {
                if self.visibility == ToolVisibility::Filtered
                    && !self
                        .policy
                        .check(&session.claims.roles, server, &info.name)
                        .is_allowed()
                {
                    continue;
                }
                let exposed = if counts[info.name.as_str()] > 1 {
                    format!("{server}__{}", info.name)
                } else {
                    info.name.clone()
                };
                out.push(self.wrap(server, st.client.clone(), info, exposed, session.clone()));
            }
        }
        out
    }

    fn wrap(
        &self,
        server: &str,
        client: McpClient,
        info: &McpToolInfo,
        exposed: String,
        session: ToolSession,
    ) -> ToolSpec {
        let server = server.to_owned();
        let tool = info.name.clone();
        let keys = self.keys.clone();
        let issuer = self.issuer.clone();
        let ttl = self.delegated_ttl_secs;
        ToolSpec {
            name: exposed,
            description: info.description.clone(),
            parameters: info.input_schema.clone(),
            call: Arc::new(move |args: Value| {
                let (server, tool, client, keys, issuer, session) = (
                    server.clone(),
                    tool.clone(),
                    client.clone(),
                    keys.clone(),
                    issuer.clone(),
                    session.clone(),
                );
                Box::pin(async move {
                    let started = Instant::now();
                    let call = async {
                        let token = keys
                            .issue_delegated_token(
                                &session.claims,
                                &session.request_id,
                                &issuer,
                                ttl,
                            )
                            .map_err(|e| McpError::Protocol(e.to_string()))?;
                        client.call_tool(&token, &tool, args.clone()).await
                    }
                    .await;
                    let (outcome, result_text, model_text) = match call {
                        Ok(ToolOutcome::Ok { text, .. }) => (CallOutcome::Ok, text.clone(), text),
                        Ok(ToolOutcome::Error { text }) => (
                            CallOutcome::Error,
                            text.clone(),
                            format!("tool error: {text}"),
                        ),
                        Ok(ToolOutcome::Denied { reason }) => (
                            CallOutcome::Denied,
                            reason.clone(),
                            format!("tool call denied by policy: {reason}"),
                        ),
                        Err(e) => (
                            CallOutcome::Error,
                            e.to_string(),
                            format!("tool error: {e}"),
                        ),
                    };
                    let latency_ms =
                        u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                    crate::telemetry::record_tool_call(
                        &session.app,
                        &session.agent,
                        &server,
                        &tool,
                        outcome,
                    );
                    session.evidence.record(ToolCallRecord {
                        server,
                        tool,
                        args,
                        outcome,
                        result_text,
                        latency_ms,
                    });
                    Ok(model_text)
                })
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(outcome: CallOutcome, text: &str) -> ToolCallRecord {
        ToolCallRecord {
            server: "docs".into(),
            tool: "search_docs".into(),
            args: json!({}),
            outcome,
            result_text: text.into(),
            latency_ms: 1,
        }
    }

    #[test]
    fn evidence_helpers() {
        let e = Evidence::from_calls(vec![
            rec(CallOutcome::Ok, "See DOC-42 for details"),
            rec(CallOutcome::Denied, "SECRET-1"),
        ]);
        assert!(e.mentions("doc-42"));
        assert!(!e.mentions("SECRET-1"));
        assert!(!e.mentions(""));
        assert_eq!(e.calls_to("docs", "search_docs").len(), 2);
        assert!(e.calls_to("docs", "other").is_empty());
        assert_eq!(e.results_text(), "See DOC-42 for details");
    }

    #[test]
    fn sse_picks_reply_event() {
        let body = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/x\"}\n\nevent: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\n";
        assert!(parse_response(body, true, 1).is_err());
        let body = "data: {\"jsonrpc\":\"2.0\",\"method\":\"n\"}\n\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"a\":1}}\n\n";
        assert_eq!(parse_response(body, true, 1).unwrap()["result"]["a"], 1);
    }

    #[test]
    fn sse_skips_replies_for_other_ids() {
        let body = "data: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"a\":7}}\n\ndata: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"a\":2}}\n\n";
        assert_eq!(parse_response(body, true, 2).unwrap()["result"]["a"], 2);
        assert!(parse_response(body, true, 3).is_err());
    }

    #[test]
    fn json_reply_must_match_id() {
        let body = r#"{"jsonrpc":"2.0","id":5,"result":{}}"#;
        assert!(parse_response(body, false, 5).is_ok());
        assert!(parse_response(body, false, 6).is_err());
        let null_err = r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"x"}}"#;
        assert!(parse_response(null_err, false, 6).is_ok());
    }

    #[test]
    fn catalog_skips_unsafe_tool_names() {
        let info = |n: &str| McpToolInfo {
            name: n.into(),
            description: String::new(),
            input_schema: json!({}),
        };
        let kept = usable_tools(
            "s",
            vec![info("ok_tool"), info("a.b"), info("x*"), info("fine-2")],
        );
        let names: Vec<_> = kept.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["ok_tool", "fine-2"]);
    }
}
