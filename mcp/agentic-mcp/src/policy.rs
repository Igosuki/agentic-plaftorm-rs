use agentic_core::auth::ROLE_PLATFORM;
use agentic_core::mcp_wire::{METHOD_TOOLS_CALL, POLICY_DENIED_CODE, POLICY_DENIED_PREFIX};
use agentic_core::policy::{Decision, Policy};
use fastmcp_rust::McpErrorCode;
use fastmcp_rust::prelude::*;
use fastmcp_rust::{JsonRpcRequest, Middleware, MiddlewareDecision};
use serde_json::Value;

/// Pure authorization decision for one JSON-RPC request.
///
/// Only `tools/call` is gated; every other method is open to any authenticated caller.
/// The `platform` role never authorizes a call, even if the policy file grants it.
pub fn decide(
    policy: &Policy,
    server: &str,
    method: &str,
    tool: Option<&str>,
    roles: &[String],
) -> Decision {
    if method != METHOD_TOOLS_CALL {
        return Decision::Allow;
    }
    let Some(tool) = tool else {
        return Decision::Deny {
            reason: "tools/call without a tool name".to_owned(),
        };
    };
    let callable: Vec<&String> = roles.iter().filter(|r| *r != ROLE_PLATFORM).collect();
    if callable.is_empty() {
        return Decision::Deny {
            reason: format!("no role permitted to call {server}.{tool}"),
        };
    }
    policy.check(&callable, server, tool)
}

/// Enforces the shared policy on `tools/call`; denials are JSON-RPC errors with code `-32003` and a `policy_denied:` message.
pub struct PolicyMiddleware {
    policy: Policy,
    server: String,
}

impl PolicyMiddleware {
    pub fn new(policy: Policy, server: impl Into<String>) -> Self {
        Self {
            policy,
            server: server.into(),
        }
    }
}

impl Middleware for PolicyMiddleware {
    fn on_request(
        &self,
        ctx: &McpContext,
        request: &JsonRpcRequest,
    ) -> McpResult<MiddlewareDecision> {
        let tool = request
            .params
            .as_ref()
            .and_then(|p| p.get("name"))
            .and_then(Value::as_str);
        let auth = ctx.auth();
        let roles = auth
            .as_ref()
            .map(|a| a.scopes.as_slice())
            .unwrap_or_default();
        let decision = decide(&self.policy, &self.server, &request.method, tool, roles);

        if request.method == METHOD_TOOLS_CALL {
            let claim = |k: &str| {
                auth.as_ref()
                    .and_then(|a| a.claims.as_ref())
                    .and_then(|c| c.get(k))
                    .and_then(Value::as_str)
                    .unwrap_or("-")
                    .to_owned()
            };
            log::info!(
                "tools/call subject={} team={} tool={}.{} decision={} request_id={}",
                auth.as_ref()
                    .and_then(|a| a.subject.as_deref())
                    .unwrap_or("-"),
                claim("team"),
                self.server,
                tool.unwrap_or("-"),
                if decision.is_allowed() {
                    "allow"
                } else {
                    "deny"
                },
                claim("request_id"),
            );
        }

        match decision {
            Decision::Allow => Ok(MiddlewareDecision::Continue),
            Decision::Deny { reason } => Err(McpError::new(
                McpErrorCode::Custom(POLICY_DENIED_CODE as i32),
                format!("{POLICY_DENIED_PREFIX} {reason}"),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> Policy {
        Policy::from_yaml_str(
            "default: deny\nroles:\n  reader: {allow: [\"docs.*\", \"tickets.get_ticket\"]}\n  engineer: {allow: [\"tickets.*\"]}\n  platform: {allow: [\"*\"]}\n",
        )
        .unwrap()
    }

    fn roles(r: &[&str]) -> Vec<String> {
        r.iter().map(|s| (*s).to_owned()).collect()
    }

    fn d(method: &str, tool: Option<&str>, r: &[&str]) -> Decision {
        decide(&policy(), "tickets", method, tool, &roles(r))
    }

    #[test]
    fn list_open_to_any_caller() {
        assert!(d("tools/list", None, &["platform"]).is_allowed());
        assert!(d("tools/list", None, &[]).is_allowed());
    }

    #[test]
    fn platform_never_calls_even_if_policy_allows() {
        assert!(!d("tools/call", Some("get_ticket"), &["platform"]).is_allowed());
    }

    #[test]
    fn platform_role_is_ignored_next_to_real_roles() {
        assert!(
            d(
                "tools/call",
                Some("create_ticket"),
                &["platform", "engineer"]
            )
            .is_allowed()
        );
        assert!(!d("tools/call", Some("create_ticket"), &["platform", "reader"]).is_allowed());
    }

    #[test]
    fn reader_vs_engineer() {
        assert!(d("tools/call", Some("get_ticket"), &["reader"]).is_allowed());
        assert!(!d("tools/call", Some("create_ticket"), &["reader"]).is_allowed());
        assert!(d("tools/call", Some("create_ticket"), &["engineer"]).is_allowed());
    }

    #[test]
    fn missing_tool_name_or_roles_denied() {
        assert!(!d("tools/call", None, &["engineer"]).is_allowed());
        assert!(!d("tools/call", Some("get_ticket"), &[]).is_allowed());
    }

    #[test]
    fn empty_policy_denies_everything() {
        let p = Policy::default();
        assert!(
            !decide(
                &p,
                "docs",
                "tools/call",
                Some("search_docs"),
                &roles(&["admin"])
            )
            .is_allowed()
        );
    }
}
