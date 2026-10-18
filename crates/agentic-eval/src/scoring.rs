use serde_json::Value;

use crate::model::{Case, ResponseSpec, TrajectoryMatch, UNCLASSIFIED};

/// What a single gateway call produced, reduced to what scoring needs.
#[derive(Debug, Clone, Default)]
pub struct Observed {
    /// `None` when the gateway answered 422 unclassified or the agent failed before an answer.
    pub agent: Option<String>,
    pub unclassified: bool,
    pub tool_calls: Vec<String>,
    pub output: Option<Value>,
}

pub fn routing_ok(expected_agent: &str, observed: &Observed) -> bool {
    if expected_agent == UNCLASSIFIED {
        observed.unclassified
    } else {
        observed.agent.as_deref() == Some(expected_agent)
    }
}

fn lcs_len(a: &[String], b: &[String]) -> usize {
    let mut prev = vec![0usize; b.len() + 1];
    for x in a {
        let mut cur = vec![0usize; b.len() + 1];
        for (j, y) in b.iter().enumerate() {
            cur[j + 1] = if x == y {
                prev[j] + 1
            } else {
                prev[j + 1].max(cur[j])
            };
        }
        prev = cur;
    }
    prev[b.len()]
}

fn multiset_intersection(expected: &[String], actual: &[String]) -> usize {
    let mut remaining: Vec<&String> = actual.iter().collect();
    expected
        .iter()
        .filter(|e| match remaining.iter().position(|a| a == e) {
            Some(i) => {
                remaining.swap_remove(i);
                true
            }
            None => false,
        })
        .count()
}

/// Empty `expected` scores 1.0 except under `Exact`, which also requires `actual` to be empty.
#[allow(clippy::cast_precision_loss)]
pub fn trajectory_score(mode: TrajectoryMatch, expected: &[String], actual: &[String]) -> f64 {
    if mode == TrajectoryMatch::Exact {
        return f64::from(u8::from(expected == actual));
    }
    if expected.is_empty() {
        return 1.0;
    }
    let matched = match mode {
        TrajectoryMatch::InOrder => lcs_len(expected, actual),
        _ => multiset_intersection(expected, actual),
    };
    matched as f64 / expected.len() as f64
}

fn lookup<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(value, |v, key| v.get(key))
}

/// Scalar leaves of `value`, one per line; keys and JSON escaping stay out of the match text.
fn flatten_text(value: &Value, out: &mut String) {
    match value {
        Value::String(s) => out.push_str(s),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::Bool(b) => out.push_str(&b.to_string()),
        Value::Array(items) => items.iter().for_each(|v| flatten_text(v, out)),
        Value::Object(map) => map.values().for_each(|v| flatten_text(v, out)),
        Value::Null => {}
    }
    out.push('\n');
}

/// Whole-term match on lowercased text: the term must not touch a letter, digit or `_`, and a digit
/// at either edge must not continue as a number or version through `.` or `,` (so `3` misses `2.0.3`,
/// `3.5` and `TCK-1003` but hits `version 3.` and `TCK-3`).
fn contains_term(haystack: &str, term: &str) -> bool {
    if term.is_empty() {
        return true;
    }
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let first_digit = term.chars().next().is_some_and(|c| c.is_ascii_digit());
    let last_digit = term.chars().last().is_some_and(|c| c.is_ascii_digit());
    haystack.match_indices(term).any(|(i, m)| {
        let before = haystack[..i].chars().rev();
        let after = haystack[i + m.len()..].chars();
        let edge_ok =
            |mut side: Box<dyn Iterator<Item = char> + '_>, edge_digit: bool| match side.next() {
                None => true,
                Some(c) if is_word(c) => false,
                Some('.' | ',') if edge_digit => !side.next().is_some_and(|d| d.is_ascii_digit()),
                Some(_) => true,
            };
        edge_ok(Box::new(before), first_digit) && edge_ok(Box::new(after), last_digit)
    })
}

/// Numbers compare as `f64` (`42 == 42.0`): exactly when both are integer-valued, within `1e-9` otherwise.
/// Everything else compares strictly, so the string `"true"` is not `true`.
fn json_eq(got: &Value, want: &Value) -> bool {
    match (got, want) {
        (Value::Number(a), Value::Number(b)) => match (a.as_f64(), b.as_f64()) {
            (Some(a), Some(b)) if a.fract() == 0.0 && b.fract() == 0.0 => a == b,
            (Some(a), Some(b)) => (a - b).abs() <= 1e-9,
            _ => a == b,
        },
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| json_eq(x, y))
        }
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(k, x)| b.get(k).is_some_and(|y| json_eq(x, y)))
        }
        _ => got == want,
    }
}

/// Fraction of satisfied checks; no checks scores 1.0.
#[allow(clippy::cast_precision_loss)]
pub fn response_score(spec: &ResponseSpec, output: Option<&Value>) -> f64 {
    let total = spec.must_contain.len() + spec.fields.len();
    if total == 0 {
        return 1.0;
    }
    let Some(output) = output else {
        return 0.0;
    };
    let mut haystack = String::new();
    flatten_text(output, &mut haystack);
    let haystack = haystack.to_lowercase();
    let contained = spec
        .must_contain
        .iter()
        .filter(|s| contains_term(&haystack, &s.to_lowercase()))
        .count();
    let fields = spec
        .fields
        .iter()
        .filter(|(path, want)| lookup(output, path).is_some_and(|got| json_eq(got, want)))
        .count();
    (contained + fields) as f64 / total as f64
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Scores {
    pub routing: bool,
    pub trajectory: f64,
    pub response: f64,
}

/// A run that missed its routing expectation scores 0 on trajectory and response: whatever it did afterwards is not the expected behaviour.
pub fn score(case: &Case, observed: &Observed) -> Scores {
    let routing = routing_ok(&case.expected_agent, observed);
    if !routing {
        return Scores {
            routing,
            trajectory: 0.0,
            response: 0.0,
        };
    }
    Scores {
        routing,
        trajectory: trajectory_score(
            case.trajectory_match,
            &case.expected_tools,
            &observed.tool_calls,
        ),
        response: response_score(&case.response, observed.output.as_ref()),
    }
}

#[allow(clippy::cast_precision_loss)]
pub fn mean(values: impl IntoIterator<Item = f64>) -> f64 {
    let (sum, n) = values
        .into_iter()
        .fold((0.0, 0usize), |(s, n), v| (s + v, n + 1));
    if n == 0 { 0.0 } else { sum / n as f64 }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn exact_requires_identical_sequences() {
        let e = v(&["a.x", "a.y"]);
        assert!((trajectory_score(TrajectoryMatch::Exact, &e, &e) - 1.0).abs() < f64::EPSILON);
        assert!(
            trajectory_score(TrajectoryMatch::Exact, &e, &v(&["a.y", "a.x"])).abs() < f64::EPSILON
        );
        assert!(
            trajectory_score(TrajectoryMatch::Exact, &e, &v(&["a.x", "a.y", "a.z"])).abs()
                < f64::EPSILON
        );
        assert!(trajectory_score(TrajectoryMatch::Exact, &[], &v(&["a.x"])).abs() < f64::EPSILON);
    }

    #[test]
    fn in_order_is_lcs_over_expected_length() {
        let e = v(&["docs.search_docs", "docs.get_doc"]);
        let full = v(&["docs.search_docs", "docs.search_docs", "docs.get_doc"]);
        assert!((trajectory_score(TrajectoryMatch::InOrder, &e, &full) - 1.0).abs() < f64::EPSILON);
        let half = v(&["docs.search_docs"]);
        assert!((trajectory_score(TrajectoryMatch::InOrder, &e, &half) - 0.5).abs() < f64::EPSILON);
        let swapped = v(&["docs.get_doc", "docs.search_docs"]);
        assert!(
            (trajectory_score(TrajectoryMatch::InOrder, &e, &swapped) - 0.5).abs() < f64::EPSILON
        );
        assert!(trajectory_score(TrajectoryMatch::InOrder, &e, &[]).abs() < f64::EPSILON);
    }

    #[test]
    fn any_order_counts_multiset_overlap() {
        let e = v(&["a.x", "a.x", "a.y"]);
        let swapped = v(&["a.y", "a.x", "a.x"]);
        assert!(
            (trajectory_score(TrajectoryMatch::AnyOrder, &e, &swapped) - 1.0).abs() < f64::EPSILON
        );
        let one_x = v(&["a.x", "a.y"]);
        assert!(
            (trajectory_score(TrajectoryMatch::AnyOrder, &e, &one_x) - 2.0 / 3.0).abs() < 1e-12
        );
    }

    #[test]
    fn empty_expected_is_satisfied_unless_exact() {
        assert!(
            (trajectory_score(TrajectoryMatch::InOrder, &[], &v(&["a.x"])) - 1.0).abs()
                < f64::EPSILON
        );
        assert!((trajectory_score(TrajectoryMatch::Exact, &[], &[]) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn response_checks_substrings_case_insensitively_and_fields_exactly() {
        let spec = ResponseSpec {
            must_contain: v(&["168", "RETENTION"]),
            fields: [
                ("grounded".to_owned(), json!(true)),
                ("meta.kind".to_owned(), json!("x")),
            ]
            .into_iter()
            .collect(),
        };
        let out =
            json!({"answer": "Retention is 168 hours", "grounded": true, "meta": {"kind": "y"}});
        assert!((response_score(&spec, Some(&out)) - 0.75).abs() < f64::EPSILON);
        assert!(response_score(&spec, None).abs() < f64::EPSILON);
    }

    #[test]
    fn numeric_fields_compare_as_numbers() {
        let score = |fields: Value, out: Value| {
            let spec = ResponseSpec {
                must_contain: Vec::new(),
                fields: fields
                    .as_object()
                    .unwrap()
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            };
            response_score(&spec, Some(&out))
        };
        assert!((score(json!({"answer": 42}), json!({"answer": 42.0})) - 1.0).abs() < f64::EPSILON);
        assert!((score(json!({"answer": 42.0}), json!({"answer": 42})) - 1.0).abs() < f64::EPSILON);
        assert!(score(json!({"answer": 42}), json!({"answer": 43.0})).abs() < f64::EPSILON);
        assert!(score(json!({"answer": 42}), json!({"answer": 42.5})).abs() < f64::EPSILON);
        let sum = 0.1_f64 + 0.2;
        assert!((score(json!({"x": 0.3}), json!({"x": sum})) - 1.0).abs() < f64::EPSILON);
        assert!(score(json!({"x": 0.3}), json!({"x": 0.31})).abs() < f64::EPSILON);
        assert!(
            (score(json!({"l": [1, {"n": 2}]}), json!({"l": [1.0, {"n": 2.0}]})) - 1.0).abs()
                < f64::EPSILON
        );
    }

    #[test]
    fn field_types_must_match() {
        let spec = ResponseSpec {
            must_contain: Vec::new(),
            fields: [("grounded".to_owned(), json!(true))].into_iter().collect(),
        };
        for out in [json!({"grounded": "true"}), json!({"grounded": 1})] {
            assert!(response_score(&spec, Some(&out)).abs() < f64::EPSILON);
        }
    }

    #[test]
    fn must_contain_respects_word_boundaries() {
        let spec = |t: &str| ResponseSpec {
            must_contain: v(&[t]),
            fields: Default::default(),
        };
        let hit = |t: &str, text: &str| {
            (response_score(&spec(t), Some(&json!({"answer": text}))) - 1.0).abs() < f64::EPSILON
        };
        assert!(!hit("3", "ticket TCK-1003 opened"));
        assert!(!hit("3", "release 2.0.3 is out"));
        assert!(!hit("3", "pi is 3.14"));
        assert!(!hit("3", "1,3"));
        assert!(!hit("log", "catalog"));
        assert!(!hit("log", "log_level"));
        assert!(hit("3", "there are 3 items"));
        assert!(hit("3", "the answer is 3."));
        assert!(hit("3", "(3)"));
        assert!(hit("3", "TCK-3 is open"));
        assert!(hit("168", "retention: 168 hours"));
        assert!(hit("168 hours", "Retention is 168 hours."));
        assert!(hit("TCK-1003", "see tck-1003, please"));
        assert!(hit("2.0.3", "release 2.0.3 is out"));
        assert!(hit("dlq", "uses a DLQ"));
    }

    #[test]
    fn must_contain_ignores_keys_and_json_escapes() {
        let spec = ResponseSpec {
            must_contain: v(&["answer"]),
            fields: Default::default(),
        };
        assert!(response_score(&spec, Some(&json!({"answer": "x"}))).abs() < f64::EPSILON);
        let spec = ResponseSpec {
            must_contain: v(&["retention"]),
            fields: Default::default(),
        };
        let out = json!({"answer": "line one\nretention is 7 days"});
        assert!((response_score(&spec, Some(&out)) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn wrong_routing_zeroes_trajectory_and_response() {
        let case: Case = serde_json::from_value(json!({
            "name": "c", "user": "u", "input": "i", "expected_agent": "docs_agent",
            "expected_tools": ["docs.search_docs"], "response": {"must_contain": ["x"]}
        }))
        .unwrap();
        let good = Observed {
            agent: Some("docs_agent".into()),
            tool_calls: v(&["docs.search_docs"]),
            output: Some(json!({"a": "x"})),
            ..Observed::default()
        };
        let s = score(&case, &good);
        assert!(
            s.routing
                && (s.trajectory - 1.0).abs() < f64::EPSILON
                && (s.response - 1.0).abs() < f64::EPSILON
        );
        for bad in [
            Observed {
                agent: Some("tickets_agent".into()),
                ..good.clone()
            },
            Observed {
                agent: None,
                unclassified: true,
                ..good.clone()
            },
            Observed {
                agent: None,
                tool_calls: good.tool_calls.clone(),
                output: None,
                ..Observed::default()
            },
        ] {
            let s = score(&case, &bad);
            assert!(!s.routing);
            assert!(s.trajectory.abs() < f64::EPSILON && s.response.abs() < f64::EPSILON);
        }
    }

    #[test]
    fn response_without_checks_is_perfect_even_without_output() {
        assert!((response_score(&ResponseSpec::default(), None) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn field_type_must_match() {
        let spec = ResponseSpec {
            must_contain: vec![],
            fields: [("grounded".to_owned(), json!(true))].into_iter().collect(),
        };
        assert!(response_score(&spec, Some(&json!({"grounded": "true"}))).abs() < f64::EPSILON);
    }

    #[test]
    fn routing_handles_unclassified() {
        let routed = Observed {
            agent: Some("docs_agent".into()),
            ..Observed::default()
        };
        let rejected = Observed {
            unclassified: true,
            ..Observed::default()
        };
        assert!(routing_ok("docs_agent", &routed));
        assert!(!routing_ok("tickets_agent", &routed));
        assert!(routing_ok(UNCLASSIFIED, &rejected));
        assert!(!routing_ok(UNCLASSIFIED, &routed));
        assert!(!routing_ok("docs_agent", &rejected));
        assert!(!routing_ok("docs_agent", &Observed::default()));
    }

    #[test]
    fn mean_of_nothing_is_zero() {
        assert!(mean([]).abs() < f64::EPSILON);
        assert!((mean([1.0, 0.0]) - 0.5).abs() < f64::EPSILON);
    }
}
