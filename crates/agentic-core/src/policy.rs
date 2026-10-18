use std::{collections::BTreeMap, path::Path};

use serde::Deserialize;

use crate::Result;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny { reason: String },
}

impl Decision {
    pub fn is_allowed(&self) -> bool {
        matches!(self, Decision::Allow)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DefaultAction {
    #[default]
    Deny,
    Allow,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RoleRules {
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Policy {
    #[serde(default)]
    pub default: DefaultAction,
    #[serde(default)]
    pub roles: BTreeMap<String, RoleRules>,
}

pub fn tool_id(server: &str, tool: &str) -> String {
    format!("{server}.{tool}")
}

impl Policy {
    pub fn from_yaml_str(yaml: &str) -> Result<Self> {
        Ok(serde_yaml::from_str(yaml)?)
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        Self::from_yaml_str(&std::fs::read_to_string(path)?)
    }

    /// Any matching `deny` across the caller's roles wins over any `allow`; otherwise `default` applies.
    pub fn check(&self, roles: &[impl AsRef<str>], server: &str, tool: &str) -> Decision {
        let id = tool_id(server, tool);
        let role_list = || {
            roles
                .iter()
                .map(AsRef::as_ref)
                .collect::<Vec<_>>()
                .join(",")
        };
        let rules = || {
            roles
                .iter()
                .filter_map(|r| self.roles.get(r.as_ref()).map(|rules| (r.as_ref(), rules)))
        };

        if let Some((role, _)) =
            rules().find(|(_, r)| r.deny.iter().any(|p| glob_match(p, &id, true)))
        {
            return Decision::Deny {
                reason: format!(
                    "role `{role}` explicitly denies `{id}` (roles: [{}])",
                    role_list()
                ),
            };
        }
        if rules().any(|(_, r)| r.allow.iter().any(|p| glob_match(p, &id, false))) {
            return Decision::Allow;
        }
        match self.default {
            DefaultAction::Allow => Decision::Allow,
            DefaultAction::Deny => Decision::Deny {
                reason: format!("no role in [{}] allows `{id}`", role_list()),
            },
        }
    }
}

/// `*` matches any run of characters; unless `cross_segment`, that run cannot contain `.`.
/// Deny patterns cross segments so an unexpected tool name cannot slip past them.
fn glob_match(pattern: &str, text: &str, cross_segment: bool) -> bool {
    if pattern == "*" {
        return true;
    }
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let mut backtrack: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && p[pi] == '*' {
            backtrack = Some((pi, ti));
            pi += 1;
        } else if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if let Some((sp, st)) = backtrack {
            if !cross_segment && t[st] == '.' {
                return false;
            }
            backtrack = Some((sp, st + 1));
            pi = sp + 1;
            ti = st + 1;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;

    const YAML: &str = r#"
default: deny
roles:
  reader:   {allow: ["docs.*", "tickets.list_tickets", "tickets.get_ticket"]}
  engineer: {allow: ["docs.*", "tickets.*"]}
  intern:   {allow: ["*"], deny: ["tickets.create_*"]}
  admin:    {allow: ["*"]}
"#;

    fn policy() -> Policy {
        Policy::from_yaml_str(YAML).unwrap()
    }

    #[test]
    fn globs() {
        assert!(glob_match("*", "a.b", false));
        assert!(glob_match("docs.*", "docs.search", false));
        assert!(!glob_match("docs.*", "docs.a.b", false));
        assert!(!glob_match("docs.*", "tickets.search", false));
        assert!(glob_match("*.get_*", "x.get_doc", false));
        assert!(glob_match("a.b", "a.b", false));
        assert!(!glob_match("a.b", "a.bc", false));
    }

    #[test]
    fn cross_segment_globs() {
        assert!(glob_match("tickets.*", "tickets.a.b", true));
        assert!(glob_match("*.create_*", "tickets.x.create_y", true));
        assert!(!glob_match("tickets.*", "docs.a.b", true));
    }

    #[test]
    fn deny_fails_closed_on_dotted_tool_names() {
        let p = Policy::from_yaml_str(
            "roles:\n  r: {allow: [\"*\"], deny: [\"tickets.*\"]}\n  a: {allow: [\"tickets.*\"]}",
        )
        .unwrap();
        assert!(!p.check(&["r"], "tickets", "evil.tool").is_allowed());
        assert!(!p.check(&["r"], "tickets", "plain").is_allowed());
        // Allow keeps segment semantics.
        assert!(!p.check(&["a"], "tickets", "evil.tool").is_allowed());
        assert!(p.check(&["a"], "tickets", "plain").is_allowed());
    }

    #[test]
    fn allow_and_default_deny() {
        let p = policy();
        assert_eq!(p.check(&["reader"], "docs", "search_docs"), Decision::Allow);
        assert_eq!(
            p.check(&["reader"], "tickets", "get_ticket"),
            Decision::Allow
        );
        let d = p.check(&["reader"], "tickets", "create_ticket");
        assert!(matches!(&d, Decision::Deny { reason }
            if reason.contains("reader") && reason.contains("tickets.create_ticket")));
        assert!(!p.check(&["nobody"], "docs", "x").is_allowed());
        assert!(!p.check(&[] as &[&str], "docs", "x").is_allowed());
    }

    #[test]
    fn any_role_allows() {
        assert!(
            policy()
                .check(&["reader", "engineer"], "tickets", "create_ticket")
                .is_allowed()
        );
    }

    #[test]
    fn deny_wins_across_roles() {
        let p = policy();
        assert!(
            !p.check(&["intern"], "tickets", "create_ticket")
                .is_allowed()
        );
        assert!(
            !p.check(&["admin", "intern"], "tickets", "create_ticket")
                .is_allowed()
        );
        assert!(p.check(&["intern"], "docs", "x").is_allowed());
    }

    #[test]
    fn default_allow() {
        let p = Policy::from_yaml_str("default: allow\nroles:\n  r: {deny: [\"x.*\"]}").unwrap();
        assert!(p.check(&["r"], "y", "t").is_allowed());
        assert!(!p.check(&["r"], "x", "t").is_allowed());
    }

    #[test]
    fn missing_default_is_deny() {
        assert!(
            !Policy::from_yaml_str("roles: {}")
                .unwrap()
                .check(&["a"], "s", "t")
                .is_allowed()
        );
    }
}
