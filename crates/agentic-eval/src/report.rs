use std::fmt::Write as _;

use serde::Serialize;

use crate::{model::Thresholds, scoring::mean};

#[derive(Debug, Clone, Serialize)]
pub struct RunRecord {
    pub case: String,
    pub run: u32,
    pub agent: Option<String>,
    pub agent_ok: bool,
    pub trajectory: f64,
    pub response: f64,
    pub latency_ms: u64,
    pub tokens: u64,
    pub attempts: u32,
    pub prompt_version: Option<String>,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CaseSummary {
    pub case: String,
    pub runs: usize,
    pub agent_ok: f64,
    pub trajectory: f64,
    pub response: f64,
    pub latency_ms: f64,
    pub tokens: f64,
    pub attempts: f64,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Totals {
    pub routing_accuracy: f64,
    pub tool_trajectory_avg: f64,
    pub response_match_avg: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub gateway: String,
    pub runs_per_case: u32,
    pub prompts: Vec<String>,
    pub cases: Vec<CaseSummary>,
    pub totals: Totals,
    pub failures: Vec<String>,
    pub records: Vec<RunRecord>,
}

pub fn summarize(records: &[RunRecord]) -> Vec<CaseSummary> {
    let mut names: Vec<&str> = Vec::new();
    for r in records {
        if !names.contains(&r.case.as_str()) {
            names.push(&r.case);
        }
    }
    names
        .into_iter()
        .map(|name| {
            let rs: Vec<&RunRecord> = records.iter().filter(|r| r.case == name).collect();
            let mut notes: Vec<String> = rs.iter().filter_map(|r| r.note.clone()).collect();
            notes.dedup();
            #[allow(clippy::cast_precision_loss)]
            CaseSummary {
                case: name.to_owned(),
                runs: rs.len(),
                agent_ok: mean(rs.iter().map(|r| f64::from(u8::from(r.agent_ok)))),
                trajectory: mean(rs.iter().map(|r| r.trajectory)),
                response: mean(rs.iter().map(|r| r.response)),
                latency_ms: mean(rs.iter().map(|r| r.latency_ms as f64)),
                tokens: mean(rs.iter().map(|r| r.tokens as f64)),
                attempts: mean(rs.iter().map(|r| f64::from(r.attempts))),
                notes,
            }
        })
        .collect()
}

pub fn totals(cases: &[CaseSummary]) -> Totals {
    Totals {
        routing_accuracy: mean(cases.iter().map(|c| c.agent_ok)),
        tool_trajectory_avg: mean(cases.iter().map(|c| c.trajectory)),
        response_match_avg: mean(cases.iter().map(|c| c.response)),
    }
}

pub fn threshold_failures(totals: &Totals, t: &Thresholds) -> Vec<String> {
    [
        (
            "routing_accuracy",
            totals.routing_accuracy,
            t.routing_accuracy,
        ),
        (
            "tool_trajectory_avg",
            totals.tool_trajectory_avg,
            t.tool_trajectory_avg,
        ),
        (
            "response_match_avg",
            totals.response_match_avg,
            t.response_match_avg,
        ),
    ]
    .into_iter()
    .filter_map(|(name, got, min)| {
        min.filter(|m| got < *m)
            .map(|m| format!("{name} {got:.2} < {m:.2}"))
    })
    .collect()
}

pub fn render(report: &Report, thresholds: &Thresholds) -> String {
    let width = report
        .cases
        .iter()
        .map(|c| c.case.len())
        .max()
        .unwrap_or(4)
        .max(4);
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{:<width$}  {:>8}  {:>10}  {:>8}  {:>10}  {:>8}  {:>8}",
        "case", "agent ok", "trajectory", "response", "latency ms", "tokens", "attempts"
    );
    for c in &report.cases {
        let _ = write!(
            out,
            "{:<width$}  {:>8}  {:>10.2}  {:>8.2}  {:>10.0}  {:>8.0}  {:>8.1}",
            c.case,
            format!("{:.0}%", c.agent_ok * 100.0),
            c.trajectory,
            c.response,
            c.latency_ms,
            c.tokens,
            c.attempts
        );
        if !c.notes.is_empty() {
            let _ = write!(out, "  ({})", c.notes.join("; "));
        }
        out.push('\n');
    }
    let t = &report.totals;
    let _ = writeln!(out);
    for (name, got, min) in [
        (
            "routing_accuracy",
            t.routing_accuracy,
            thresholds.routing_accuracy,
        ),
        (
            "tool_trajectory_avg",
            t.tool_trajectory_avg,
            thresholds.tool_trajectory_avg,
        ),
        (
            "response_match_avg",
            t.response_match_avg,
            thresholds.response_match_avg,
        ),
    ] {
        let verdict = match min {
            Some(m) if got >= m => format!(">= {m:.2}  PASS"),
            Some(m) => format!(">= {m:.2}  FAIL"),
            None => "no threshold".to_owned(),
        };
        let _ = writeln!(out, "{name:<20} {got:>5.2}  {verdict}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(case: &str, ok: bool, traj: f64, resp: f64) -> RunRecord {
        RunRecord {
            case: case.into(),
            run: 1,
            agent: None,
            agent_ok: ok,
            trajectory: traj,
            response: resp,
            latency_ms: 100,
            tokens: 10,
            attempts: 1,
            prompt_version: None,
            note: None,
        }
    }

    #[test]
    fn averages_runs_per_case_then_cases() {
        let records = [
            rec("a", true, 1.0, 1.0),
            rec("a", false, 0.0, 0.5),
            rec("b", true, 1.0, 1.0),
        ];
        let cases = summarize(&records);
        assert_eq!(cases.len(), 2);
        assert!((cases[0].agent_ok - 0.5).abs() < f64::EPSILON);
        let t = totals(&cases);
        assert!((t.routing_accuracy - 0.75).abs() < f64::EPSILON);
        assert!((t.tool_trajectory_avg - 0.75).abs() < f64::EPSILON);
        assert!((t.response_match_avg - 0.875).abs() < f64::EPSILON);
    }

    #[test]
    fn reports_only_missed_thresholds() {
        let t = Totals {
            routing_accuracy: 0.8,
            tool_trajectory_avg: 0.9,
            response_match_avg: 0.6,
        };
        let th = Thresholds {
            routing_accuracy: Some(0.9),
            tool_trajectory_avg: Some(0.8),
            response_match_avg: None,
        };
        let f = threshold_failures(&t, &th);
        assert_eq!(f, ["routing_accuracy 0.80 < 0.90"]);
    }
}
