//! Display agent: lays a guard-validated output out as an A2UI surface.
//!
//! The model emits components only. Every value shown is bound to a JSON pointer into the output, and
//! the data model sent to the client is the output itself, so the model cannot alter or invent data.

use std::collections::{HashMap, HashSet};

use adk_ui::{A2UI_BASIC_CATALOG_ID, A2uiSchemaVersion, A2uiValidator, UiSurface};
use agentic_core::auth::Claims;
use serde_json::Value;

use crate::{
    engine::{AgentEngine, Attribution, TokenUsage, TurnError, TurnRequest},
    guard::{self, AttemptOutcome, AttemptRecord},
};

pub const AGENT: &str = "display_agent";
pub const PROMPT_ID: &str = "display_agent";
pub const PROTOCOL: &str = "a2ui";
pub const VERSION: &str = "0.9";

/// Subset of the A2UI basic catalog that renders data; inputs and media players have nothing to bind to.
const COMPONENTS: &[&str] = &[
    "Text", "Icon", "Image", "Row", "Column", "List", "Card", "Tabs", "Divider",
];

const PROTOCOL_RULES: &str = r#"Respond ONLY with a JSON object {"components": [...]} (no prose, no markdown fences).

Components use the A2UI v0.9 flat form, e.g. {"id": "title", "component": "Text", "text": {"path": "/answer"}, "variant": "h2"}.
- Allowed component types: Text, Icon, Image, Row, Column, List, Card, Tabs, Divider.
- Exactly one component has id "root"; every other component must be reachable from it through `child`, `children` or `tabs[].child`. Children are referenced by id, never inlined.
- Every value shown comes from the data model: a dynamic value is {"path": "<JSON pointer>"} naming a location in the output, e.g. {"path": "/sources/0/title"}. Never copy output values into components. Literal strings are only for static labels such as headings.
- To repeat a component for each element of an array, give the List or Column `children` as a template {"componentId": "<id of the item component>", "path": "/sources"}. The item component (and its descendants) then use paths relative to the element, without a leading slash, e.g. {"path": "title"}.
- There is no Table component: build a table as a header Row of Text labels plus a template List whose item component is a Row of Text cells bound to relative paths.
- Literal strings are allowed only as static labels: a Text with variant h1-h4 or caption, or a Tabs title. A label is at most 40 characters, contains none of `${`, `](`, `<`, `http`, and must not equal or contain any value from the output. Body Text, Image `url` and every other data-bearing property must be a {"path": ...} binding (or a formatString call whose literal parts are labels). Icon `name` is a bound path or a standard icon name.
- Only reference paths that exist in the output value you are given."#;

pub struct DisplayRequest<'a> {
    pub engine: &'a dyn AgentEngine,
    pub model_spec: &'a str,
    pub preamble: &'a str,
    pub temperature: Option<f64>,
    pub schema: &'a Value,
    pub output: &'a Value,
    pub max_attempts: u32,
    pub backoff_base_ms: u64,
    pub app: &'a str,
    pub use_case: &'a str,
    pub prompt_version: &'a str,
    pub caller: Option<Claims>,
    pub request_id: &'a str,
}

#[derive(Debug)]
pub struct Surface {
    pub surface_id: String,
    pub messages: Vec<Value>,
}

#[derive(Debug)]
pub struct DisplayResult {
    pub surface: Option<Surface>,
    pub attempts: Vec<AttemptRecord>,
    pub usage: TokenUsage,
}

fn preamble(template: &str, schema: &Value) -> String {
    format!(
        "{template}\n\n{PROTOCOL_RULES}\n\nJSON Schema of the output you are laying out:\n{}",
        serde_json::to_string_pretty(schema).unwrap_or_default()
    )
}

fn first_message(output: &Value) -> String {
    format!(
        "Lay out this output as a surface. Output value:\n{}",
        serde_json::to_string_pretty(output).unwrap_or_default()
    )
}

fn feedback(errors: &[String]) -> String {
    format!(
        "Your previous layout was rejected:\n- {}\nRespond again with ONLY the corrected {{\"components\": [...]}} JSON object.",
        errors.join("\n- ")
    )
}

fn parse_components(text: &str) -> Result<Vec<Value>, Vec<String>> {
    let value = guard::extract_json(text)
        .ok_or_else(|| vec!["response contained no JSON object".to_owned()])?;
    match value.get("components") {
        Some(Value::Array(c)) if !c.is_empty() => Ok(c.clone()),
        _ => Err(vec![
            "response must be an object with a non-empty `components` array".to_owned(),
        ]),
    }
}

/// Where relative paths resolve: the first element of the enclosing list template.
#[derive(Clone, PartialEq, Eq, Hash)]
enum Scope {
    Root,
    Item(String),
    /// The template's collection is empty, so relative paths have nothing to resolve against.
    EmptyItem,
}

fn absolute(path: &str, scope: &Scope) -> Result<Option<String>, String> {
    if path.starts_with('/') {
        return Ok(Some(path.to_owned()));
    }
    match scope {
        Scope::Root => Err(format!(
            "`{path}` is not a JSON pointer: it must start with `/` outside a list template"
        )),
        Scope::Item(base) => Ok(Some(format!("{base}/{path}"))),
        Scope::EmptyItem => Ok(None),
    }
}

/// Pointers inside `${...}` string interpolations.
fn interpolations(s: &str) -> impl Iterator<Item = &str> {
    s.split("${")
        .skip(1)
        .filter_map(|rest| Some(&rest[..rest.find('}')?]))
}

/// The parts of `s` outside `${...}`.
fn literal_parts(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut rest = s;
    while let Some(start) = rest.find("${") {
        parts.push(&rest[..start]);
        match rest[start..].find('}') {
            Some(end) => rest = &rest[start + end + 1..],
            None => return parts,
        }
    }
    parts.push(rest);
    parts
}

fn string_values(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) => out.push(s.to_lowercase()),
        Value::Array(items) => items.iter().for_each(|i| string_values(i, out)),
        Value::Object(map) => map.values().for_each(|i| string_values(i, out)),
        _ => {}
    }
}

const LABEL_MAX_CHARS: usize = 40;
const LABEL_FORBIDDEN: &[&str] = &["${", "](", "<", "http"];
/// Shorter output strings would match inside almost any label.
const CONTAINMENT_MIN_CHARS: usize = 3;
const LABEL_VARIANTS: &[&str] = &["h1", "h2", "h3", "h4", "caption"];

const ICONS: &[&str] = &[
    "accountCircle",
    "add",
    "arrowBack",
    "arrowForward",
    "attachFile",
    "calendarToday",
    "call",
    "camera",
    "check",
    "close",
    "delete",
    "download",
    "edit",
    "event",
    "error",
    "fastForward",
    "favorite",
    "favoriteOff",
    "folder",
    "help",
    "home",
    "info",
    "locationOn",
    "lock",
    "lockOpen",
    "mail",
    "menu",
    "moreVert",
    "moreHoriz",
    "notificationsOff",
    "notifications",
    "pause",
    "payment",
    "person",
    "phone",
    "photo",
    "play",
    "print",
    "refresh",
    "rewind",
    "search",
    "send",
    "settings",
    "share",
    "shoppingCart",
    "skipNext",
    "skipPrevious",
    "star",
    "starHalf",
    "starOff",
    "stop",
    "upload",
    "visibility",
    "visibilityOff",
    "volumeDown",
    "volumeMute",
    "volumeOff",
    "volumeUp",
    "warning",
];

const ALIGN: &[&str] = &["start", "center", "end", "stretch"];
const JUSTIFY: &[&str] = &[
    "start",
    "center",
    "end",
    "spaceBetween",
    "spaceAround",
    "spaceEvenly",
    "stretch",
];

/// Properties of each allowed component: structural ones are checked elsewhere, enum ones take one of the listed values.
fn enum_props(kind: &str) -> &'static [(&'static str, &'static [&'static str])] {
    match kind {
        "Text" => &[(
            "variant",
            &["h1", "h2", "h3", "h4", "h5", "caption", "body"],
        )],
        "Image" => &[
            ("fit", &["contain", "cover", "fill", "none", "scale-down"]),
            (
                "variant",
                &[
                    "icon",
                    "avatar",
                    "smallFeature",
                    "mediumFeature",
                    "largeFeature",
                    "header",
                ],
            ),
        ],
        "Row" | "Column" => &[("justify", JUSTIFY), ("align", ALIGN)],
        "List" => &[("direction", &["vertical", "horizontal"]), ("align", ALIGN)],
        "Divider" => &[("axis", &["horizontal", "vertical"])],
        _ => &[],
    }
}

/// Properties holding data or structure rather than an enum choice.
fn value_props(kind: &str) -> &'static [&'static str] {
    match kind {
        "Text" => &["text"],
        "Image" => &["url"],
        "Icon" => &["name"],
        "Row" | "Column" | "List" => &["children"],
        "Card" => &["child"],
        "Tabs" => &["tabs"],
        _ => &[],
    }
}

struct Checker<'a> {
    output: &'a Value,
    /// Lowercased string values of the output, which labels must not reproduce.
    output_strings: Vec<String>,
    errors: Vec<String>,
    bindings: usize,
}

impl Checker<'_> {
    fn err(&mut self, id: &str, msg: impl std::fmt::Display) {
        self.errors.push(format!("component `{id}`: {msg}"));
    }

    fn bind(&mut self, id: &str, path: &str, scope: &Scope) {
        self.bindings += 1;
        match absolute(path, scope) {
            Err(e) => self.err(id, e),
            Ok(Some(p)) if self.output.pointer(&p).is_none() => self.err(
                id,
                format!("path `{path}` does not resolve in the output (looked up `{p}`)"),
            ),
            Ok(_) => {}
        }
    }

    /// A static label may name a field but never carry output data.
    fn label_error(&self, label: &str) -> Option<String> {
        let lower = label.to_lowercase();
        if label.chars().count() > LABEL_MAX_CHARS {
            return Some(format!(
                "literal `{label}` is longer than {LABEL_MAX_CHARS} characters"
            ));
        }
        if let Some(bad) = LABEL_FORBIDDEN.iter().find(|f| lower.contains(*f)) {
            return Some(format!("literal `{label}` contains `{bad}`"));
        }
        let trimmed = lower.trim();
        if trimmed.is_empty() {
            return None;
        }
        self.output_strings
            .iter()
            .find(|v| {
                let v = v.trim();
                !v.is_empty()
                    && (v == trimmed
                        || (v.chars().count() >= CONTAINMENT_MIN_CHARS && trimmed.contains(v)))
            })
            .map(|_| {
                format!("literal `{label}` reproduces a value from the output; bind it with a path")
            })
    }

    /// `literal_ok` is whether a plain literal may stand in this slot.
    fn dynamic_string(&mut self, id: &str, prop: &str, v: &Value, literal_ok: bool, scope: &Scope) {
        match v {
            Value::String(s) if literal_ok => {
                if let Some(e) = self.label_error(s) {
                    self.err(id, e);
                }
            }
            Value::String(_) => self.err(
                id,
                format!("`{prop}` must be a {{\"path\": ...}} binding, not a literal"),
            ),
            Value::Object(m) if m.len() == 1 && m.get("path").is_some_and(Value::is_string) => {
                self.bind(id, m["path"].as_str().unwrap_or_default(), scope);
            }
            Value::Object(m) if m.get("call").and_then(Value::as_str) == Some("formatString") => {
                let template = m
                    .get("args")
                    .and_then(|a| a.get("value"))
                    .and_then(Value::as_str);
                let Some(template) = template else {
                    return self.err(
                        id,
                        format!("`{prop}` formatString needs a string `args.value`"),
                    );
                };
                for part in literal_parts(template) {
                    if let Some(e) = self.label_error(part) {
                        self.err(id, e);
                    }
                }
                for p in interpolations(template) {
                    self.bind(id, p, scope);
                }
            }
            _ => self.err(
                id,
                format!("`{prop}` must be a {{\"path\": ...}} binding or a formatString call"),
            ),
        }
    }

    fn check_props(&mut self, id: &str, kind: &str, component: &Value, scope: &Scope) {
        let Some(props) = component.as_object() else {
            return;
        };
        let enums = enum_props(kind);
        let values = value_props(kind);
        for (prop, v) in props {
            let prop = prop.as_str();
            if matches!(prop, "id" | "component" | "weight") {
                continue;
            }
            if let Some((_, allowed)) = enums.iter().find(|(p, _)| *p == prop) {
                if !v.as_str().is_some_and(|s| allowed.contains(&s)) {
                    self.err(
                        id,
                        format!("`{prop}` must be one of {}", allowed.join(", ")),
                    );
                }
            } else if !values.contains(&prop) {
                self.err(id, format!("unknown property `{prop}` for {kind}"));
            }
        }
        match kind {
            "Text" => {
                let label = component
                    .get("variant")
                    .and_then(Value::as_str)
                    .is_some_and(|v| LABEL_VARIANTS.contains(&v));
                match component.get("text") {
                    Some(t) => self.dynamic_string(id, "text", t, label, scope),
                    None => self.err(id, "`text` is required"),
                }
            }
            "Image" => match component.get("url") {
                Some(u) => self.dynamic_string(id, "url", u, false, scope),
                None => self.err(id, "`url` is required"),
            },
            "Icon" => match component.get("name") {
                Some(Value::String(n)) if ICONS.contains(&n.as_str()) => {}
                Some(Value::String(n)) => self.err(id, format!("`{n}` is not an icon name")),
                Some(v) => self.dynamic_string(id, "name", v, false, scope),
                None => self.err(id, "`name` is required"),
            },
            "Tabs" => {
                let tabs = component.get("tabs").and_then(Value::as_array);
                for tab in tabs.into_iter().flatten() {
                    if let Some(t) = tab.get("title") {
                        self.dynamic_string(id, "tabs[].title", t, true, scope);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Children of `component` with the scope each is rendered in; list templates open a new scope.
fn children_of(
    component: &Value,
    scope: &Scope,
    checker: &mut Checker<'_>,
) -> Vec<(String, Scope)> {
    let id = component["id"].as_str().unwrap_or_default();
    let mut found = Vec::new();
    if let Some(c) = component.get("child").and_then(Value::as_str) {
        found.push((c.to_owned(), scope.clone()));
    }
    if let Some(tabs) = component.get("tabs").and_then(Value::as_array) {
        let tabbed = tabs
            .iter()
            .filter_map(|t| t.get("child").and_then(Value::as_str));
        found.extend(tabbed.map(|c| (c.to_owned(), scope.clone())));
    }
    match component.get("children") {
        Some(Value::Array(ids)) => {
            found.extend(
                ids.iter()
                    .filter_map(Value::as_str)
                    .map(|c| (c.to_owned(), scope.clone())),
            );
        }
        Some(Value::Object(template)) => {
            let item = template.get("componentId").and_then(Value::as_str);
            let path = template.get("path").and_then(Value::as_str);
            let (Some(item), Some(path)) = (item, path) else {
                checker.err(id, "a children template needs `componentId` and `path`");
                return found;
            };
            checker.bind(id, path, scope);
            let child_scope = match absolute(path, scope) {
                Ok(Some(collection)) => match checker.output.pointer(&collection) {
                    Some(Value::Array(items)) if !items.is_empty() => {
                        Scope::Item(format!("{collection}/0"))
                    }
                    Some(Value::Array(_)) | None => Scope::EmptyItem,
                    Some(_) => {
                        checker.err(
                            id,
                            format!("template path `{path}` is not an array in the output"),
                        );
                        Scope::EmptyItem
                    }
                },
                Ok(None) | Err(_) => Scope::EmptyItem,
            };
            found.push((item.to_owned(), child_scope));
        }
        _ => {}
    }
    found
}

fn index_components<'a>(
    components: &'a [Value],
    checker: &mut Checker<'_>,
) -> HashMap<&'a str, &'a Value> {
    let mut by_id: HashMap<&str, &Value> = HashMap::new();
    for c in components {
        let Some(id) = c.get("id").and_then(Value::as_str) else {
            checker
                .errors
                .push("every component needs a string `id`".to_owned());
            continue;
        };
        match c.get("component").and_then(Value::as_str) {
            Some(kind) if COMPONENTS.contains(&kind) => {}
            Some(kind) => checker.err(
                id,
                format!(
                    "type `{kind}` is not allowed (use {})",
                    COMPONENTS.join(", ")
                ),
            ),
            None => checker.err(id, "`component` must be a type name string"),
        }
        if by_id.insert(id, c).is_some() {
            checker
                .errors
                .push(format!("duplicate component id `{id}`"));
        }
    }
    by_id
}

/// Ids reachable from `root`, checking each component in every scope it is rendered in.
fn walk_from_root(by_id: &HashMap<&str, &Value>, checker: &mut Checker<'_>) -> HashSet<String> {
    let mut visited = HashSet::new();
    let mut stack = vec![("root".to_owned(), Scope::Root)];
    while let Some((id, scope)) = stack.pop() {
        if !visited.insert((id.clone(), scope.clone())) {
            continue;
        }
        let Some(component) = by_id.get(id.as_str()) else {
            continue;
        };
        if let Some(kind) = component.get("component").and_then(Value::as_str)
            && COMPONENTS.contains(&kind)
        {
            checker.check_props(&id, kind, component, &scope);
        }
        for (child, child_scope) in children_of(component, &scope, checker) {
            if by_id.contains_key(child.as_str()) {
                stack.push((child, child_scope));
            } else {
                checker.err(&id, format!("references unknown component `{child}`"));
            }
        }
    }
    visited.into_iter().map(|(id, _)| id).collect()
}

/// Structure and data bindings of `components` against `output`; all violations are returned together.
fn check_components(components: &[Value], output: &Value) -> Result<(), Vec<String>> {
    let mut output_strings = Vec::new();
    string_values(output, &mut output_strings);
    let mut checker = Checker {
        output,
        output_strings,
        errors: Vec::new(),
        bindings: 0,
    };
    let by_id = index_components(components, &mut checker);
    if !by_id.contains_key("root") {
        checker.errors.push("no component has id `root`".to_owned());
        return Err(checker.errors);
    }

    let reached = walk_from_root(&by_id, &mut checker);
    let mut orphans: Vec<&str> = by_id
        .keys()
        .copied()
        .filter(|id| !reached.contains(*id))
        .collect();
    orphans.sort_unstable();
    if !orphans.is_empty() {
        checker.errors.push(format!(
            "components not reachable from `root`: {}",
            orphans.join(", ")
        ));
    }
    if checker.bindings == 0 {
        checker.errors.push(
            "no component binds a `path` into the output; show data through paths, not literals"
                .to_owned(),
        );
    }
    if checker.errors.is_empty() {
        Ok(())
    } else {
        Err(checker.errors)
    }
}

/// `createSurface`, `updateDataModel` (the output, whole) and `updateComponents`, validated against A2UI v0.9.
fn build_messages(
    surface_id: &str,
    components: Vec<Value>,
    output: &Value,
) -> Result<Vec<Value>, CheckError> {
    let surface = UiSurface::new(surface_id, A2UI_BASIC_CATALOG_ID, components)
        .with_data_model(Some(output.clone()))
        .with_send_data_model(false);
    let validator = A2uiValidator::new().map_err(|e| CheckError::Infra(e.to_string()))?;
    let mut messages = Vec::new();
    for m in surface.to_a2ui_messages() {
        validator
            .validate_message(&m, A2uiSchemaVersion::V0_9)
            .map_err(|errs| {
                CheckError::Violation(errs.iter().map(ToString::to_string).collect())
            })?;
        messages.push(serde_json::to_value(&m).map_err(|e| CheckError::Infra(e.to_string()))?);
    }
    Ok(messages)
}

#[derive(Debug)]
enum CheckError {
    /// The model's layout is wrong; a re-prompt can fix it.
    Violation(Vec<String>),
    /// Our own tooling failed; a re-prompt cannot help.
    Infra(String),
}

fn check(text: &str, surface_id: &str, output: &Value) -> Result<Vec<Value>, CheckError> {
    let components = parse_components(text).map_err(CheckError::Violation)?;
    check_components(&components, output).map_err(CheckError::Violation)?;
    build_messages(surface_id, components, output)
}

pub async fn run(req: DisplayRequest<'_>) -> DisplayResult {
    let surface_id = format!("ui-{}", req.request_id);
    let mut result = DisplayResult {
        surface: None,
        attempts: Vec::new(),
        usage: TokenUsage::default(),
    };
    let caller = req.caller.as_ref();
    let turn = TurnRequest {
        model_spec: req.model_spec.to_owned(),
        preamble: preamble(req.preamble, req.schema),
        temperature: req.temperature,
        max_turns: 1,
        tools: Vec::new(),
        caller: req.caller.clone(),
        request_id: req.request_id.to_owned(),
        attribution: Attribution {
            user: caller.map(|c| c.sub.clone()).unwrap_or_default(),
            team: caller.map(|c| c.team.clone()).unwrap_or_default(),
            application: req.app.to_owned(),
            use_case: req.use_case.to_owned(),
            agent: AGENT.to_owned(),
            prompt_version: req.prompt_version.to_owned(),
            request_id: req.request_id.to_owned(),
        },
    };
    let mut conversation = req.engine.conversation();
    let mut next_message = first_message(req.output);
    let mut infra_failures = 0u32;
    let mut tooling_failed = false;

    for attempt in 1..=req.max_attempts.max(1) {
        let (outcome, detail, used) = match req
            .engine
            .chat(&turn, &mut conversation, next_message.clone())
            .await
        {
            Ok(resp) => {
                let (outcome, detail) = match check(&resp.text, &surface_id, req.output) {
                    Ok(messages) => {
                        result.surface = Some(Surface {
                            surface_id: surface_id.clone(),
                            messages,
                        });
                        (AttemptOutcome::Ok, None)
                    }
                    Err(CheckError::Violation(errors)) => {
                        next_message = feedback(&errors);
                        (AttemptOutcome::SchemaViolation, Some(errors.join("; ")))
                    }
                    Err(CheckError::Infra(e)) => {
                        tooling_failed = true;
                        (AttemptOutcome::InfraError, Some(e))
                    }
                };
                (outcome, detail, resp.usage)
            }
            Err(e @ TurnError::MaxTurns { .. }) => {
                (AttemptOutcome::MaxTurns, Some(e.to_string()), e.usage())
            }
            Err(e @ TurnError::Rejected { .. }) => {
                (AttemptOutcome::Rejected, Some(e.to_string()), e.usage())
            }
            Err(e) => (AttemptOutcome::InfraError, Some(e.to_string()), e.usage()),
        };
        result.usage.input_tokens += used.input_tokens;
        result.usage.output_tokens += used.output_tokens;

        crate::telemetry::record_attempt(req.app, AGENT, req.prompt_version, outcome);
        if let Some(d) = &detail {
            tracing::warn!(
                agent = AGENT,
                prompt_version = req.prompt_version,
                attempt,
                outcome = outcome.as_str(),
                detail = %d,
                "display attempt failed"
            );
        }
        result.attempts.push(AttemptRecord {
            attempt,
            outcome,
            detail,
        });
        match outcome {
            AttemptOutcome::Ok | AttemptOutcome::Rejected => break,
            AttemptOutcome::InfraError => infra_failures += 1,
            _ => infra_failures = 0,
        }
        if tooling_failed || attempt >= req.max_attempts {
            break;
        }
        if outcome == AttemptOutcome::InfraError {
            tokio::time::sleep(guard::backoff(req.backoff_base_ms, infra_failures)).await;
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, sync::Mutex};

    use async_trait::async_trait;
    use serde_json::json;

    use super::*;
    use crate::engine::{Conversation, TurnOutput};

    fn output() -> Value {
        json!({
            "answer": "Use the dead-letter queue.",
            "grounded": true,
            "sources": [{"id": "cfg-001", "title": "Configuration Reference"}]
        })
    }

    fn good_layout() -> Value {
        json!({"components": [
            {"id": "root", "component": "Column", "children": ["answer", "sources"]},
            {"id": "answer", "component": "Text", "text": {"path": "/answer"}},
            {"id": "sources", "component": "List",
             "children": {"componentId": "source", "path": "/sources"}},
            {"id": "source", "component": "Text", "text": {"path": "title"}}
        ]})
    }

    fn comps(v: &Value) -> Vec<Value> {
        v["components"].as_array().unwrap().clone()
    }

    #[test]
    fn accepts_absolute_and_template_relative_bindings() {
        check_components(&comps(&good_layout()), &output()).unwrap();
    }

    #[test]
    fn rejects_missing_path() {
        let out = json!({"tickets": [{"title": "t"}]});
        let layout = json!({"components": [
            {"id": "root", "component": "List",
             "children": {"componentId": "row", "path": "/tickets"}},
            {"id": "row", "component": "Text", "text": {"path": "/tickets/0/id"}}
        ]});
        let errs = check_components(&comps(&layout), &out).unwrap_err();
        assert!(errs.iter().any(|e| e.contains("/tickets/0/id")), "{errs:?}");

        let relative = json!({"components": [
            {"id": "root", "component": "List",
             "children": {"componentId": "row", "path": "/tickets"}},
            {"id": "row", "component": "Text", "text": {"path": "id"}}
        ]});
        let errs = check_components(&comps(&relative), &out).unwrap_err();
        assert!(errs.iter().any(|e| e.contains("`id`")), "{errs:?}");
    }

    #[test]
    fn rejects_non_pointer_and_missing_collection() {
        let layout = json!({"components": [
            {"id": "root", "component": "Text", "text": {"path": "answer"}}
        ]});
        let errs = check_components(&comps(&layout), &output()).unwrap_err();
        assert!(
            errs.iter().any(|e| e.contains("not a JSON pointer")),
            "{errs:?}"
        );

        let layout = json!({"components": [
            {"id": "root", "component": "List",
             "children": {"componentId": "row", "path": "/answer"}},
            {"id": "row", "component": "Text", "text": {"path": "x"}}
        ]});
        let errs = check_components(&comps(&layout), &output()).unwrap_err();
        assert!(errs.iter().any(|e| e.contains("not an array")), "{errs:?}");
    }

    #[test]
    fn rejects_structural_problems() {
        let literal_only = json!({"components": [
            {"id": "root", "component": "Text", "text": "Use the dead-letter queue."}
        ]});
        let errs = check_components(&comps(&literal_only), &output()).unwrap_err();
        assert!(
            errs.iter().any(|e| e.contains("binds a `path`")),
            "{errs:?}"
        );

        let bad = json!({"components": [
            {"id": "root", "component": "Column", "children": ["ghost"]},
            {"id": "orphan", "component": "Text", "text": {"path": "/answer"}},
            {"id": "form", "component": "TextField", "label": "x"}
        ]});
        let errs = check_components(&comps(&bad), &output())
            .unwrap_err()
            .join("\n");
        assert!(errs.contains("unknown component `ghost`"), "{errs}");
        assert!(errs.contains("not reachable"), "{errs}");
        assert!(errs.contains("`TextField` is not allowed"), "{errs}");

        let errs =
            check_components(&[json!({"id": "a", "component": "Text"})], &output()).unwrap_err();
        assert!(errs[0].contains("`root`"));
    }

    fn errors_of(layout: &Value) -> String {
        check_components(&comps(layout), &output())
            .unwrap_err()
            .join("\n")
    }

    #[test]
    fn short_static_labels_are_accepted() {
        let layout = json!({"components": [
            {"id": "root", "component": "Column", "children": ["head", "cap", "answer"]},
            {"id": "head", "component": "Text", "variant": "h2", "text": "Answer"},
            {"id": "cap", "component": "Text", "variant": "caption",
             "text": {"call": "formatString", "args": {"value": "Grounded: ${/grounded}"}}},
            {"id": "answer", "component": "Text", "text": {"path": "/answer"}}
        ]});
        check_components(&comps(&layout), &output()).unwrap();
    }

    #[test]
    fn literal_data_is_rejected() {
        let body = json!({"components": [
            {"id": "root", "component": "Column", "children": ["a", "b"]},
            {"id": "a", "component": "Text", "text": "Short note"},
            {"id": "b", "component": "Text", "text": {"path": "/answer"}}
        ]});
        assert!(errors_of(&body).contains("not a literal"));

        let image = json!({"components": [
            {"id": "root", "component": "Column", "children": ["i", "b"]},
            {"id": "i", "component": "Image", "url": "logo.png"},
            {"id": "b", "component": "Text", "text": {"path": "/answer"}}
        ]});
        assert!(errors_of(&image).contains("`url`"));

        for label in [
            "Use the dead-letter queue.",
            "Answer: use the DEAD-LETTER queue.",
            "Configuration Reference",
            "Read more: https://x.y",
            "Fine </b>",
            "a label that is far too long to be a static heading",
        ] {
            let layout = json!({"components": [
                {"id": "root", "component": "Column", "children": ["h", "b"]},
                {"id": "h", "component": "Text", "variant": "h3", "text": label},
                {"id": "b", "component": "Text", "text": {"path": "/answer"}}
            ]});
            assert!(errors_of(&layout).contains("literal"), "{label}");
        }

        let format = json!({"components": [
            {"id": "root", "component": "Text", "variant": "caption",
             "text": {"call": "formatString", "args": {"value": "Use the dead-letter queue. ${/answer}"}}}
        ]});
        assert!(errors_of(&format).contains("reproduces"));
    }

    #[test]
    fn icons_are_named_or_bound() {
        let ok = json!({"components": [
            {"id": "root", "component": "Row", "children": ["i", "t"]},
            {"id": "i", "component": "Icon", "name": "check"},
            {"id": "t", "component": "Text", "text": {"path": "/answer"}}
        ]});
        check_components(&comps(&ok), &output()).unwrap();
        let bad = json!({"components": [
            {"id": "root", "component": "Row", "children": ["i", "t"]},
            {"id": "i", "component": "Icon", "name": "secret"},
            {"id": "t", "component": "Text", "text": {"path": "/answer"}}
        ]});
        assert!(errors_of(&bad).contains("not an icon name"));
    }

    #[test]
    fn a_component_reached_from_two_scopes_is_checked_in_each() {
        let layout = json!({"components": [
            {"id": "root", "component": "Column", "children": ["cell", "list"]},
            {"id": "list", "component": "List",
             "children": {"componentId": "cell", "path": "/sources"}},
            {"id": "cell", "component": "Text", "text": {"path": "title"}}
        ]});
        assert!(errors_of(&layout).contains("not a JSON pointer"));
    }

    #[test]
    fn empty_collection_skips_relative_checks() {
        let out = json!({"tickets": []});
        let layout = json!({"components": [
            {"id": "root", "component": "List",
             "children": {"componentId": "row", "path": "/tickets"}},
            {"id": "row", "component": "Text", "text": {"path": "title"}}
        ]});
        check_components(&comps(&layout), &out).unwrap();
    }

    #[test]
    fn interpolated_pointers_are_checked() {
        let layout = json!({"components": [
            {"id": "root", "component": "Text",
             "text": {"call": "formatString", "args": {"value": "Grounded: ${/missing}"}}}
        ]});
        let errs = check_components(&comps(&layout), &output()).unwrap_err();
        assert!(errs.iter().any(|e| e.contains("/missing")), "{errs:?}");
    }

    #[test]
    fn messages_carry_the_output_as_the_data_model() {
        let msgs = build_messages("s1", comps(&good_layout()), &output()).unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0]["createSurface"]["surfaceId"], "s1");
        assert_eq!(msgs[1]["updateDataModel"]["path"], "/");
        assert_eq!(msgs[1]["updateDataModel"]["value"], output());
        assert_eq!(msgs[2]["updateComponents"]["components"][0]["id"], "root");
    }

    struct Scripted {
        replies: Mutex<VecDeque<Result<String, TurnError>>>,
        seen: Mutex<Vec<String>>,
    }

    impl Scripted {
        fn new(replies: Vec<Result<String, TurnError>>) -> Self {
            Self {
                replies: Mutex::new(replies.into()),
                seen: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl AgentEngine for Scripted {
        fn name(&self) -> &'static str {
            "scripted"
        }

        fn check_model(&self, _spec: &str) -> crate::Result<()> {
            Ok(())
        }

        fn conversation(&self) -> Conversation {
            Conversation::new(())
        }

        async fn chat(
            &self,
            _req: &TurnRequest,
            _conv: &mut Conversation,
            message: String,
        ) -> Result<TurnOutput, TurnError> {
            self.seen.lock().unwrap().push(message);
            let usage = TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
            };
            match self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .expect("script exhausted")
            {
                Ok(text) => Ok(TurnOutput { text, usage }),
                Err(e) => Err(e),
            }
        }

        async fn prompt(
            &self,
            _req: &TurnRequest,
            _message: String,
        ) -> Result<TurnOutput, TurnError> {
            unreachable!("display agent uses chat")
        }
    }

    async fn run_with(engine: &Scripted, max_attempts: u32) -> DisplayResult {
        let out = output();
        run(DisplayRequest {
            engine,
            model_spec: "test:m",
            preamble: "Lay it out.",
            temperature: None,
            schema: &json!({"type": "object"}),
            output: &out,
            max_attempts,
            backoff_base_ms: 1,
            app: "test",
            use_case: "docs_agent",
            prompt_version: "1.0.0",
            caller: None,
            request_id: "r1",
        })
        .await
    }

    #[tokio::test]
    async fn retries_with_the_violation_then_succeeds() {
        let bad = json!({"components": [
            {"id": "root", "component": "Text", "text": {"path": "/nope"}}
        ]});
        let engine = Scripted::new(vec![Ok(bad.to_string()), Ok(good_layout().to_string())]);
        let r = run_with(&engine, 2).await;

        let surface = r.surface.expect("rendered");
        assert_eq!(surface.surface_id, "ui-r1");
        assert_eq!(surface.messages.len(), 3);
        let outcomes: Vec<_> = r.attempts.iter().map(|a| a.outcome).collect();
        assert_eq!(
            outcomes,
            [AttemptOutcome::SchemaViolation, AttemptOutcome::Ok]
        );
        assert_eq!(r.usage.total(), 30);
        let seen = engine.seen.lock().unwrap();
        assert!(seen[0].contains("dead-letter"));
        assert!(seen[1].contains("/nope"), "{}", seen[1]);
    }

    #[tokio::test]
    async fn exhausting_attempts_yields_no_surface() {
        let engine = Scripted::new(vec![
            Ok("not json".to_owned()),
            Err(TurnError::Infra {
                message: "boom".into(),
                usage: TokenUsage::default(),
            }),
        ]);
        let r = run_with(&engine, 2).await;
        assert!(r.surface.is_none());
        assert_eq!(r.attempts.len(), 2);
        assert_eq!(r.attempts[1].outcome, AttemptOutcome::InfraError);
        assert_eq!(r.attempts[1].detail.as_deref(), Some("boom"));
    }
}
