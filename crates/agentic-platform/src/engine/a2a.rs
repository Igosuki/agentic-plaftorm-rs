//! Engine that delegates each turn to a remote A2A agent (`model: a2a:<remote>`).

use std::{collections::BTreeMap, time::Instant};

use a2a_rs::{
    JsonRpcClient, Transport,
    domain::{A2AError, Message, Part, Role, SendCompletion, Task, TaskState, part},
};
use agentic_core::auth::{Claims, JwtKeys, downscope_roles};
use async_trait::async_trait;
use buffa_types::google::protobuf::Struct;
use serde_json::{Value, json};

use super::{AgentEngine, Conversation, TokenUsage, TurnError, TurnOutput, TurnRequest};
use crate::{
    PlatformError, Result,
    config::{RemoteAgentConfig, RemoteAuth},
    telemetry,
};

const PROBE_TIMEOUT_SECS: u64 = 5;

pub struct A2aEngine {
    app: String,
    remotes: BTreeMap<String, RemoteAgentConfig>,
    keys: JwtKeys,
    issuer: String,
    token_ttl_secs: i64,
}

struct Turn {
    request: String,
    answer: String,
}

/// The transcript is replayed inline on every call and is the only history a remote sees; `id` only correlates calls.
struct A2aConversation {
    id: String,
    turns: Vec<Turn>,
}

struct Reply {
    text: String,
    usage: TokenUsage,
}

#[derive(Debug)]
enum CallError {
    /// HTTP 401/403: the credentials will not become valid by retrying.
    Unauthorized,
    /// The remote refused the request itself (task `rejected`, invalid params); retrying the same request cannot help.
    Rejected(String),
    Remote(String),
    Transport(String),
}

impl CallError {
    fn label(&self) -> &'static str {
        match self {
            Self::Unauthorized => "unauthorized",
            Self::Rejected(_) => "rejected",
            Self::Remote(_) => "remote_failed",
            Self::Transport(_) => "error",
        }
    }
}

/// JSON-RPC errors that blame the request, not the remote's health.
fn is_request_error(e: &A2AError) -> bool {
    matches!(
        e,
        A2AError::InvalidParams(_)
            | A2AError::InvalidRequest(_)
            | A2AError::MethodNotFound(_)
            | A2AError::ValidationError { .. }
            | A2AError::ContextAccessDenied { .. }
            | A2AError::UnsupportedOperation(_)
            | A2AError::ContentTypeNotSupported(_)
    )
}

impl A2aEngine {
    pub fn new(
        app: impl Into<String>,
        remotes: BTreeMap<String, RemoteAgentConfig>,
        keys: JwtKeys,
        issuer: impl Into<String>,
        token_ttl_secs: i64,
    ) -> Self {
        Self {
            app: app.into(),
            remotes,
            keys,
            issuer: issuer.into(),
            token_ttl_secs,
        }
    }

    fn resolve(&self, spec: &str) -> std::result::Result<(&str, &RemoteAgentConfig), String> {
        let name = spec
            .strip_prefix("a2a:")
            .ok_or_else(|| format!("a2a engine models look like `a2a:<remote>`, got `{spec}`"))?;
        self.remotes
            .get_key_value(name)
            .map(|(k, v)| (k.as_str(), v))
            .ok_or_else(|| {
                let known: Vec<&str> = self.remotes.keys().map(String::as_str).collect();
                format!(
                    "remote agent `{name}` is not configured in remote_agents (configured: {})",
                    known.join(", ")
                )
            })
    }

    async fn call(
        &self,
        req: &TurnRequest,
        text: String,
        conversation_id: &str,
    ) -> std::result::Result<TurnOutput, TurnError> {
        let infra = |message: String| TurnError::Infra {
            message,
            usage: TokenUsage::default(),
        };
        let (name, remote) = self.resolve(&req.model_spec).map_err(infra)?;
        let started = Instant::now();
        let result = self.send(name, remote, req, text, conversation_id).await;
        let outcome = result.as_ref().map_or_else(CallError::label, |_| "ok");
        telemetry::record_a2a_call(&self.app, name, outcome, started.elapsed().as_secs_f64());
        match result {
            Ok(r) => Ok(TurnOutput {
                text: r.text,
                usage: r.usage,
            }),
            Err(CallError::Unauthorized) => Err(TurnError::Rejected {
                message: format!("remote `{name}` rejected credentials (401)"),
                usage: TokenUsage::default(),
            }),
            Err(CallError::Rejected(m)) => Err(TurnError::Rejected {
                message: format!("remote `{name}` rejected the request: {m}"),
                usage: TokenUsage::default(),
            }),
            Err(CallError::Remote(m)) => Err(infra(format!("remote `{name}` failed: {m}"))),
            Err(CallError::Transport(m)) => Err(infra(format!("remote `{name}` unreachable: {m}"))),
        }
    }

    async fn send(
        &self,
        name: &str,
        remote: &RemoteAgentConfig,
        req: &TurnRequest,
        text: String,
        conversation_id: &str,
    ) -> std::result::Result<Reply, CallError> {
        let token = match remote.auth {
            RemoteAuth::None => None,
            RemoteAuth::Delegated => {
                let caller = req.caller.as_ref().ok_or_else(|| {
                    CallError::Transport(format!(
                        "remote `{name}` needs delegated auth but the turn has no caller"
                    ))
                })?;
                Some(
                    self.keys
                        .issue_delegated_token_for(
                            &Claims {
                                roles: downscope_roles(
                                    &caller.roles,
                                    remote.delegate_roles.as_deref(),
                                ),
                                ..caller.clone()
                            },
                            &remote.audience,
                            &req.request_id,
                            &self.issuer,
                            self.token_ttl_secs,
                        )
                        .map_err(|e| CallError::Transport(e.to_string()))?,
                )
            }
        };
        let client = match &token {
            Some(t) => JsonRpcClient::try_with_auth(remote.url.clone(), t.clone()),
            None => JsonRpcClient::try_new(remote.url.clone()),
        }
        .map_err(|e| CallError::Transport(e.to_string()))?
        .with_timeout(remote.timeout_secs);

        // Fresh per call: the inline transcript is the single source of history, so a remote that keeps
        // per-context state must not see earlier turns twice.
        let context_id = uuid::Uuid::new_v4().to_string();
        let mut metadata = json!({
            "request_id": req.request_id,
            "conversation_id": conversation_id,
        });
        if let Some(skill) = &remote.skill {
            metadata["skill"] = json!(skill);
        }
        let metadata: Struct =
            serde_json::from_value(metadata).map_err(|e| CallError::Transport(e.to_string()))?;
        let message = Message::builder()
            .role(Role::ROLE_USER)
            .parts(vec![Part::text(text)])
            .message_id(uuid::Uuid::new_v4().to_string())
            .context_id(context_id.clone())
            .metadata(metadata)
            .build();

        let task = client
            .send_task_message(
                None,
                &message,
                Some(&context_id),
                None,
                SendCompletion::WhenSettled,
            )
            .await;
        match task {
            Ok(task) => extract_reply(&task),
            Err(e) if is_request_error(&e) => Err(CallError::Rejected(e.to_string())),
            Err(e) => {
                let msg = e.to_string();
                // The a2a-rs client cannot tell a non-JSON 401 from a corrupt body.
                if msg.contains("invalid JSON-RPC response")
                    && probe_unauthorized(&remote.url, token.as_deref()).await
                {
                    Err(CallError::Unauthorized)
                } else {
                    Err(CallError::Transport(msg))
                }
            }
        }
    }
}

async fn probe_unauthorized(url: &str, token: Option<&str>) -> bool {
    let mut rb = reqwest::Client::new()
        .post(url)
        .timeout(std::time::Duration::from_secs(PROBE_TIMEOUT_SECS))
        .json(&json!({}));
    if let Some(t) = token {
        rb = rb.bearer_auth(t);
    }
    matches!(rb.send().await, Ok(r) if matches!(r.status().as_u16(), 401 | 403))
}

fn format_turn(preamble: &str, turns: &[Turn], message: &str) -> String {
    if turns.is_empty() {
        return format!("{preamble}\n\n{message}");
    }
    let mut out = format!("{preamble}\n\nConversation so far:\n");
    for t in turns {
        out.push_str(&format!("user: {}\nassistant: {}\n", t.request, t.answer));
    }
    out.push_str(&format!("\nuser: {message}"));
    out
}

/// Lossy by necessity: protobuf `Value` carries only doubles, so every integral float (a genuine `42.0` too) becomes an integer, because serde rejects `42.0` for integer fields.
fn normalize_numbers(v: Value) -> Value {
    match v {
        Value::Number(n) => match n.as_f64() {
            Some(f) if n.is_f64() && f.fract() == 0.0 && f.abs() < 9_007_199_254_740_992.0 => {
                if f >= 0.0 {
                    json!(f as u64)
                } else {
                    json!(f as i64)
                }
            }
            _ => Value::Number(n),
        },
        Value::Array(a) => Value::Array(a.into_iter().map(normalize_numbers).collect()),
        Value::Object(o) => Value::Object(
            o.into_iter()
                .map(|(k, v)| (k, normalize_numbers(v)))
                .collect(),
        ),
        other => other,
    }
}

fn state_name(task: &Task) -> TaskState {
    task.status
        .as_option()
        .and_then(|s| s.state.as_known())
        .unwrap_or(TaskState::TASK_STATE_UNSPECIFIED)
}

#[derive(Default)]
struct Collected {
    texts: Vec<String>,
    data: Option<Value>,
}

impl Collected {
    fn add(&mut self, parts: &[Part]) {
        for p in parts {
            match &p.content {
                Some(part::Content::Text(t)) if !t.trim().is_empty() => {
                    if !self.texts.contains(t) {
                        self.texts.push(t.clone());
                    }
                }
                Some(part::Content::Data(d)) => {
                    if let Ok(v) = serde_json::to_value(&**d)
                        && self.data.is_none()
                        && v.get("output").is_some()
                    {
                        self.data = Some(normalize_numbers(v));
                    }
                }
                _ => {}
            }
        }
    }
}

fn status_text(task: &Task) -> Option<String> {
    let mut c = Collected::default();
    if let Some(m) = task.status.as_option().and_then(|s| s.message.as_option()) {
        c.add(&m.parts);
    }
    (!c.texts.is_empty()).then(|| c.texts.join("\n"))
}

/// Settled task to the answer text; a `rejected` task is `CallError::Rejected`, any other non-completed state `Remote`.
fn extract_reply(task: &Task) -> std::result::Result<Reply, CallError> {
    let state = state_name(task);
    if state != TaskState::TASK_STATE_COMPLETED {
        let detail = status_text(task).unwrap_or_default();
        let message = match state {
            TaskState::TASK_STATE_FAILED
            | TaskState::TASK_STATE_REJECTED
            | TaskState::TASK_STATE_CANCELED => {
                let name = format!("{state:?}");
                if detail.is_empty() {
                    format!("task ended in state {name}")
                } else {
                    detail
                }
            }
            other => format!("task ended in unsupported state {other:?}: {detail}"),
        };
        return Err(if state == TaskState::TASK_STATE_REJECTED {
            CallError::Rejected(message)
        } else {
            CallError::Remote(message)
        });
    }
    let mut c = Collected::default();
    if let Some(m) = task.status.as_option().and_then(|s| s.message.as_option()) {
        c.add(&m.parts);
    }
    for a in &task.artifacts {
        c.add(&a.parts);
    }
    if c.texts.is_empty() && c.data.is_none() {
        for m in task
            .history
            .iter()
            .filter(|m| m.role.as_known() == Some(Role::ROLE_AGENT))
        {
            c.add(&m.parts);
        }
    }
    if let Some(d) = c.data {
        let usage = &d["trace"]["usage"];
        let tokens = |k: &str| usage[k].as_f64().map_or(0, |f| f.max(0.0) as u64);
        return Ok(Reply {
            text: serde_json::to_string(&d["output"])
                .map_err(|e| CallError::Remote(e.to_string()))?,
            usage: TokenUsage {
                input_tokens: tokens("input_tokens"),
                output_tokens: tokens("output_tokens"),
            },
        });
    }
    if c.texts.is_empty() {
        return Err(CallError::Remote(
            "task completed without any content".into(),
        ));
    }
    Ok(Reply {
        text: c.texts.join("\n"),
        usage: TokenUsage::default(),
    })
}

#[async_trait]
impl AgentEngine for A2aEngine {
    fn name(&self) -> &'static str {
        "a2a"
    }

    fn check_model(&self, spec: &str) -> Result<()> {
        self.resolve(spec).map(drop).map_err(PlatformError::Model)
    }

    fn conversation(&self) -> Conversation {
        Conversation::new(A2aConversation {
            id: uuid::Uuid::new_v4().to_string(),
            turns: Vec::new(),
        })
    }

    async fn chat(
        &self,
        req: &TurnRequest,
        conv: &mut Conversation,
        message: String,
    ) -> std::result::Result<TurnOutput, TurnError> {
        let state = conv
            .state_mut::<A2aConversation>()
            .ok_or_else(|| TurnError::Infra {
                message: "conversation was not created by the a2a engine".into(),
                usage: TokenUsage::default(),
            })?;
        let text = format_turn(&req.preamble, &state.turns, &message);
        let conversation_id = state.id.clone();
        let out = self.call(req, text, &conversation_id).await?;
        if let Some(state) = conv.state_mut::<A2aConversation>() {
            state.turns.push(Turn {
                request: message,
                answer: out.text.clone(),
            });
        }
        Ok(out)
    }

    async fn prompt(
        &self,
        req: &TurnRequest,
        message: String,
    ) -> std::result::Result<TurnOutput, TurnError> {
        let text = format_turn(&req.preamble, &[], &message);
        self.call(req, text, &uuid::Uuid::new_v4().to_string())
            .await
    }
}

#[cfg(test)]
mod tests {
    use a2a_rs::domain::TaskStatus;

    use super::*;

    fn agent_msg(parts: Vec<Part>) -> Message {
        Message::builder()
            .role(Role::ROLE_AGENT)
            .parts(parts)
            .message_id("m".into())
            .build()
    }

    fn data_part(v: Value) -> Part {
        Part::data(serde_json::from_value(v).unwrap())
    }

    fn task(state: TaskState, status_msg: Option<Message>) -> Task {
        let mut t = Task::new("t".into(), "c".into());
        t.status = TaskStatus::new(state, status_msg).into();
        t
    }

    #[test]
    fn first_turn_is_preamble_then_message() {
        assert_eq!(format_turn("P", &[], "Q"), "P\n\nQ");
    }

    #[test]
    fn follow_up_replays_earlier_answers_and_feedback() {
        let turns = vec![Turn {
            request: "Q".into(),
            answer: "bad".into(),
        }];
        let t = format_turn("P", &turns, "fix it");
        assert!(t.starts_with("P\n\nConversation so far:\n"));
        assert!(t.contains("user: Q\nassistant: bad\n"));
        assert!(t.ends_with("user: fix it"));
    }

    #[test]
    fn data_part_output_wins_and_reports_trace_usage() {
        let m = agent_msg(vec![
            Part::text("ignored".into()),
            data_part(json!({
                "output": {"value": 42, "tags": ["a"]},
                "trace": {"usage": {"input_tokens": 12, "output_tokens": 3, "cost_usd": 0.0}}
            })),
        ]);
        let r = extract_reply(&task(TaskState::TASK_STATE_COMPLETED, Some(m))).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&r.text).unwrap(),
            json!({"value": 42, "tags": ["a"]})
        );
        assert_eq!(
            r.usage,
            TokenUsage {
                input_tokens: 12,
                output_tokens: 3
            }
        );
    }

    #[test]
    fn text_parts_are_deduplicated_across_status_and_artifacts() {
        let mut t = task(
            TaskState::TASK_STATE_COMPLETED,
            Some(agent_msg(vec![Part::text("hello".into())])),
        );
        t.artifacts.push(a2a_rs::domain::Artifact {
            parts: vec![Part::text("hello".into()), Part::text("world".into())],
            ..Default::default()
        });
        assert_eq!(extract_reply(&t).unwrap().text, "hello\nworld");
    }

    #[test]
    fn falls_back_to_agent_history() {
        let mut t = task(TaskState::TASK_STATE_COMPLETED, None);
        t.history.push(Message::user_text("q".into(), "1".into()));
        t.history.push(agent_msg(vec![Part::text("answer".into())]));
        assert_eq!(extract_reply(&t).unwrap().text, "answer");
    }

    #[test]
    fn failed_states_carry_the_remote_message() {
        let t = task(
            TaskState::TASK_STATE_FAILED,
            Some(agent_msg(vec![Part::text("agent_failed: boom".into())])),
        );
        assert!(
            matches!(extract_reply(&t), Err(CallError::Remote(m)) if m == "agent_failed: boom")
        );
        let t = task(TaskState::TASK_STATE_REJECTED, None);
        assert!(matches!(extract_reply(&t), Err(CallError::Rejected(m)) if m.contains("REJECTED")));
    }

    #[test]
    fn integral_floats_become_integers() {
        let v = normalize_numbers(json!({"a": 42.0, "b": [1.5, -3.0], "c": "x"}));
        assert_eq!(v, json!({"a": 42, "b": [1.5, -3], "c": "x"}));
    }

    #[test]
    fn model_spec_must_name_a_configured_remote() {
        let e = A2aEngine::new(
            "app",
            BTreeMap::from([(
                "ops".to_owned(),
                serde_yaml::from_str::<RemoteAgentConfig>("url: http://x").unwrap(),
            )]),
            JwtKeys::from_secret(b"0123456789abcdef0123456789abcdef"),
            "iss",
            60,
        );
        assert!(e.check_model("a2a:ops").is_ok());
        assert!(
            e.check_model("a2a:nope")
                .unwrap_err()
                .to_string()
                .contains("nope")
        );
        assert!(e.check_model("ollama:x").is_err());
    }
}
