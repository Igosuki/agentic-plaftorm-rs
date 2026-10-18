use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
};

use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};
use serde::Serialize;
use tracing_subscriber::{EnvFilter, util::SubscriberInitExt};

use crate::{guard::AttemptOutcome, mcp::CallOutcome};

/// Honours `RUST_LOG` and `LOG_FORMAT=json`; repeated calls are no-ops.
pub fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    let _ = if std::env::var("LOG_FORMAT").is_ok_and(|v| v.eq_ignore_ascii_case("json")) {
        builder.json().finish().try_init()
    } else {
        builder.finish().try_init()
    };
}

const DURATION_BUCKETS: &[f64] = &[
    0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 20.0, 30.0, 60.0, 120.0,
];

/// Installs the process-global recorder once and returns its handle.
pub fn prometheus_handle() -> PrometheusHandle {
    static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();
    HANDLE
        .get_or_init(|| {
            let recorder = PrometheusBuilder::new()
                .set_buckets_for_metric(
                    Matcher::Full("agentic_request_duration_seconds".into()),
                    DURATION_BUCKETS,
                )
                .expect("non-empty histogram buckets")
                .set_buckets_for_metric(
                    Matcher::Full("agentic_a2a_call_duration_seconds".into()),
                    DURATION_BUCKETS,
                )
                .expect("non-empty histogram buckets")
                .build_recorder();
            let handle = recorder.handle();
            if metrics::set_global_recorder(recorder).is_err() {
                tracing::warn!("a metrics recorder was already installed; /metrics will be empty");
            }
            handle
        })
        .clone()
}

pub fn record_attempt(app: &str, agent: &str, prompt_version: &str, outcome: AttemptOutcome) {
    metrics::counter!("agentic_attempts_total",
        "app" => app.to_owned(), "agent" => agent.to_owned(),
        "prompt_version" => prompt_version.to_owned(), "outcome" => outcome.as_str())
    .increment(1);
}

pub fn record_tool_call(app: &str, agent: &str, server: &str, tool: &str, outcome: CallOutcome) {
    metrics::counter!("agentic_tool_calls_total",
        "app" => app.to_owned(), "agent" => agent.to_owned(),
        "server" => server.to_owned(), "tool" => tool.to_owned(), "outcome" => outcome.as_str())
    .increment(1);
}

/// `outcome` is one of `ok|remote_failed|unauthorized|error`.
pub fn record_a2a_call(app: &str, remote: &str, outcome: &str, seconds: f64) {
    metrics::counter!("agentic_a2a_calls_total",
        "app" => app.to_owned(), "remote" => remote.to_owned(), "outcome" => outcome.to_owned())
    .increment(1);
    metrics::histogram!("agentic_a2a_call_duration_seconds",
        "app" => app.to_owned(), "remote" => remote.to_owned())
    .record(seconds);
}

pub fn record_intent(app: &str, method: &str, result: &str) {
    metrics::counter!("agentic_intent_total",
        "app" => app.to_owned(), "method" => method.to_owned(), "result" => result.to_owned())
    .increment(1);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestOutcome {
    Ok,
    Unclassified,
    AgentFailed,
    RateLimited,
    InternalError,
}

impl RequestOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Unclassified => "unclassified",
            Self::AgentFailed => "agent_failed",
            Self::RateLimited => "rate_limited",
            Self::InternalError => "internal_error",
        }
    }
}

#[derive(Debug, Clone)]
pub struct RequestRecord {
    pub app: String,
    pub user: String,
    pub team: String,
    pub use_case: String,
    pub agent: String,
    pub prompt_version: String,
    pub model: String,
    pub outcome: RequestOutcome,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
    pub latency_ms: u64,
    pub hallucinations: u64,
    pub schema_violations: u64,
}

/// `agentic_cost_usd_total` is exported as a gauge because `metrics` counters are integral; PromQL treats it as a monotonic sum.
pub fn record_request(r: &RequestRecord) {
    let (app, team, use_case, agent) = (
        r.app.clone(),
        r.team.clone(),
        r.use_case.clone(),
        r.agent.clone(),
    );
    metrics::counter!("agentic_requests_total",
        "app" => app.clone(), "team" => team.clone(), "use_case" => use_case.clone(),
        "agent" => agent.clone(), "prompt_version" => r.prompt_version.clone(),
        "outcome" => r.outcome.as_str())
    .increment(1);
    metrics::histogram!("agentic_request_duration_seconds", "app" => app.clone(), "agent" => agent.clone())
        .record(r.latency_ms as f64 / 1000.0);
    record_usage(&UsageRecord {
        app: &r.app,
        team: &r.team,
        use_case: &r.use_case,
        agent: &r.agent,
        model: &r.model,
        input_tokens: r.input_tokens,
        output_tokens: r.output_tokens,
        cost_usd: r.cost_usd,
    });
}

/// Token and cost attribution for model calls that are not a request's specialist run (the coordinator).
pub struct UsageRecord<'a> {
    pub app: &'a str,
    pub team: &'a str,
    pub use_case: &'a str,
    pub agent: &'a str,
    pub model: &'a str,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
}

pub fn record_usage(u: &UsageRecord<'_>) {
    if u.input_tokens + u.output_tokens == 0 && u.cost_usd == 0.0 {
        return;
    }
    for (direction, n) in [("input", u.input_tokens), ("output", u.output_tokens)] {
        metrics::counter!("agentic_tokens_total",
            "app" => u.app.to_owned(), "team" => u.team.to_owned(), "use_case" => u.use_case.to_owned(),
            "agent" => u.agent.to_owned(), "model" => u.model.to_owned(), "direction" => direction)
        .increment(n);
    }
    metrics::gauge!("agentic_cost_usd_total",
        "app" => u.app.to_owned(), "team" => u.team.to_owned(), "use_case" => u.use_case.to_owned(),
        "agent" => u.agent.to_owned(), "model" => u.model.to_owned())
    .increment(u.cost_usd);
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct UserMetrics {
    pub user_id: String,
    pub team: String,
    pub requests: u64,
    pub successes: u64,
    pub errors: u64,
    pub hallucinations: u64,
    pub schema_violations: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
    pub avg_latency_ms: f64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct TeamMetrics {
    pub team: String,
    pub application: String,
    pub use_case: String,
    pub requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
}

#[derive(Default)]
pub struct MetricsStore {
    users: Mutex<HashMap<String, UserMetrics>>,
    teams: Mutex<HashMap<(String, String, String), TeamMetrics>>,
}

impl MetricsStore {
    pub fn record(&self, r: &RequestRecord) {
        {
            let mut users = self.users.lock().unwrap_or_else(|e| e.into_inner());
            let u = users.entry(r.user.clone()).or_insert_with(|| UserMetrics {
                user_id: r.user.clone(),
                team: r.team.clone(),
                ..UserMetrics::default()
            });
            u.requests += 1;
            if r.outcome == RequestOutcome::Ok {
                u.successes += 1;
            } else {
                u.errors += 1;
            }
            u.hallucinations += r.hallucinations;
            u.schema_violations += r.schema_violations;
            u.input_tokens += r.input_tokens;
            u.output_tokens += r.output_tokens;
            u.cost_usd += r.cost_usd;
            u.avg_latency_ms += (r.latency_ms as f64 - u.avg_latency_ms) / u.requests as f64;
        }
        let mut teams = self.teams.lock().unwrap_or_else(|e| e.into_inner());
        let t = teams
            .entry((r.team.clone(), r.app.clone(), r.use_case.clone()))
            .or_insert_with(|| TeamMetrics {
                team: r.team.clone(),
                application: r.app.clone(),
                use_case: r.use_case.clone(),
                ..TeamMetrics::default()
            });
        t.requests += 1;
        t.input_tokens += r.input_tokens;
        t.output_tokens += r.output_tokens;
        t.cost_usd += r.cost_usd;
    }

    /// Adds tokens and cost without counting a request.
    pub fn add_usage(&self, user: &str, u: &UsageRecord<'_>) {
        {
            let mut users = self.users.lock().unwrap_or_else(|e| e.into_inner());
            let m = users.entry(user.to_owned()).or_insert_with(|| UserMetrics {
                user_id: user.to_owned(),
                team: u.team.to_owned(),
                ..UserMetrics::default()
            });
            m.input_tokens += u.input_tokens;
            m.output_tokens += u.output_tokens;
            m.cost_usd += u.cost_usd;
        }
        let mut teams = self.teams.lock().unwrap_or_else(|e| e.into_inner());
        let t = teams
            .entry((u.team.to_owned(), u.app.to_owned(), u.use_case.to_owned()))
            .or_insert_with(|| TeamMetrics {
                team: u.team.to_owned(),
                application: u.app.to_owned(),
                use_case: u.use_case.to_owned(),
                ..TeamMetrics::default()
            });
        t.input_tokens += u.input_tokens;
        t.output_tokens += u.output_tokens;
        t.cost_usd += u.cost_usd;
    }

    pub fn user(&self, id: &str, team: &str) -> UserMetrics {
        self.users
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned()
            .unwrap_or_else(|| UserMetrics {
                user_id: id.to_owned(),
                team: team.to_owned(),
                ..UserMetrics::default()
            })
    }

    pub fn teams(&self) -> Vec<TeamMetrics> {
        let mut v: Vec<_> = self
            .teams
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();
        v.sort_by(|a, b| {
            (&a.team, &a.application, &a.use_case).cmp(&(&b.team, &b.application, &b.use_case))
        });
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(outcome: RequestOutcome, latency_ms: u64) -> RequestRecord {
        RequestRecord {
            app: "a".into(),
            user: "alice".into(),
            team: "t".into(),
            use_case: "u".into(),
            agent: "ag".into(),
            prompt_version: "1".into(),
            model: "m".into(),
            outcome,
            input_tokens: 10,
            output_tokens: 5,
            cost_usd: 0.5,
            latency_ms,
            hallucinations: 1,
            schema_violations: 2,
        }
    }

    #[test]
    fn aggregates_per_user_and_team() {
        let s = MetricsStore::default();
        s.record(&rec(RequestOutcome::Ok, 100));
        s.record(&rec(RequestOutcome::AgentFailed, 300));
        let u = s.user("alice", "t");
        assert_eq!((u.requests, u.successes, u.errors), (2, 1, 1));
        assert_eq!((u.hallucinations, u.schema_violations), (2, 4));
        assert!((u.avg_latency_ms - 200.0).abs() < 1e-9);
        assert!((u.cost_usd - 1.0).abs() < 1e-9);
        let t = s.teams();
        assert_eq!((t.len(), t[0].requests, t[0].input_tokens), (1, 2, 20));
        assert_eq!(s.user("bob", "x").requests, 0);
    }

    #[test]
    fn add_usage_does_not_count_requests() {
        let s = MetricsStore::default();
        s.add_usage(
            "alice",
            &UsageRecord {
                app: "a",
                team: "t",
                use_case: "none",
                agent: "coordinator",
                model: "m",
                input_tokens: 7,
                output_tokens: 3,
                cost_usd: 0.25,
            },
        );
        let u = s.user("alice", "t");
        assert_eq!((u.requests, u.input_tokens, u.output_tokens), (0, 7, 3));
        assert!((u.cost_usd - 0.25).abs() < 1e-9);
        let t = s.teams();
        assert_eq!(
            (t[0].requests, t[0].input_tokens, t[0].use_case.as_str()),
            (0, 7, "none")
        );
    }
}
