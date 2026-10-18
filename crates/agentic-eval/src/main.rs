mod client;
mod model;
mod report;
mod scoring;

use std::{
    collections::HashMap,
    path::PathBuf,
    process::ExitCode,
    time::{Duration, Instant},
};

use clap::Parser;
use client::{Gateway, RunReply};
use model::{Case, EvalConfig};
use report::{Report, RunRecord};
use scoring::Observed;
use serde_json::Value;

#[derive(Parser)]
#[command(
    about = "Golden-set evaluator for an agentic-platform gateway; exits 1 when a threshold is missed, 2 on infrastructure errors"
)]
struct Args {
    #[arg(long)]
    gateway: String,
    #[arg(long)]
    golden: PathBuf,
    #[arg(long)]
    config: PathBuf,
    /// Runs per case; defaults to `runs` in the eval config, else 1.
    #[arg(long)]
    runs: Option<u32>,
    /// Run only the case with this name.
    #[arg(long)]
    case: Option<String>,
    #[arg(long)]
    json_report: Option<PathBuf>,
    /// Make `<prompt id>` live at `<version>` for the run, then restore the previous live version; neither move enters rollback history. Repeatable; needs --admin-key.
    #[arg(long = "set-live", value_name = "ID=VERSION", requires = "admin_key")]
    set_live: Vec<String>,
    #[arg(long)]
    admin_key: Option<String>,
    #[arg(long, default_value_t = 300)]
    timeout_secs: u64,
}

fn read<T: serde::de::DeserializeOwned>(
    path: &PathBuf,
    parse: fn(&str) -> Result<T, String>,
) -> Result<T, String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    parse(&text).map_err(|e| format!("{}: {e}", path.display()))
}

fn tool_names(body: &Value) -> Vec<String> {
    body["trace"]["tool_calls"]
        .as_array()
        .map(|calls| {
            calls
                .iter()
                .map(|c| {
                    format!(
                        "{}.{}",
                        c["server"].as_str().unwrap_or("?"),
                        c["tool"].as_str().unwrap_or("?")
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

fn observe(reply: &RunReply) -> (Observed, Option<String>) {
    match reply {
        RunReply::Answered(b) => (
            Observed {
                agent: b["agent"].as_str().map(str::to_owned),
                unclassified: false,
                tool_calls: tool_names(b),
                output: Some(b["output"].clone()),
            },
            None,
        ),
        RunReply::Unclassified(_) => (
            Observed {
                unclassified: true,
                ..Observed::default()
            },
            Some("unclassified".into()),
        ),
        RunReply::AgentFailed(b) => (
            Observed {
                tool_calls: tool_names(b),
                ..Observed::default()
            },
            Some("agent_failed".into()),
        ),
    }
}

fn trace_of(reply: &RunReply) -> &Value {
    match reply {
        RunReply::Answered(b) | RunReply::Unclassified(b) | RunReply::AgentFailed(b) => &b["trace"],
    }
}

async fn run_once(gw: &Gateway, token: &str, case: &Case, run: u32) -> Result<RunRecord, String> {
    let started = Instant::now();
    let reply = gw.run(token, &case.input).await?;
    let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let (observed, note) = observe(&reply);
    let scores = scoring::score(case, &observed);
    let trace = trace_of(&reply);
    Ok(RunRecord {
        case: case.name.clone(),
        run,
        agent: observed.agent,
        agent_ok: scores.routing,
        trajectory: scores.trajectory,
        response: scores.response,
        latency_ms,
        tokens: trace["usage"]["input_tokens"].as_u64().unwrap_or(0)
            + trace["usage"]["output_tokens"].as_u64().unwrap_or(0),
        attempts: u32::try_from(trace["attempts"].as_array().map_or(0, Vec::len))
            .unwrap_or(u32::MAX),
        prompt_version: match &reply {
            RunReply::Answered(b) => b["prompt"]["version"]
                .as_str()
                .map(|v| format!("{}@{v}", b["prompt"]["id"].as_str().unwrap_or("?"))),
            _ => None,
        },
        note,
    })
}

async fn run_cases(
    gw: &Gateway,
    cfg: &EvalConfig,
    cases: &[Case],
    runs: u32,
) -> Result<Vec<RunRecord>, String> {
    let mut tokens: HashMap<String, String> = HashMap::new();
    let mut records = Vec::new();
    for case in cases {
        let key = cfg.api_key(&case.user).to_owned();
        if !tokens.contains_key(&key) {
            let t = gw.token(&key).await?;
            tokens.insert(key.clone(), t);
        }
        for run in 1..=runs {
            let rec = run_once(gw, &tokens[&key], case, run).await?;
            eprintln!(
                "{} run {run}/{runs}: traj {:.2} resp {:.2}",
                rec.case, rec.trajectory, rec.response
            );
            records.push(rec);
        }
    }
    Ok(records)
}

fn parse_set_live(specs: &[String]) -> Result<Vec<(String, String)>, String> {
    specs
        .iter()
        .map(|s| {
            s.split_once('=')
                .filter(|(id, v)| !id.is_empty() && !v.is_empty())
                .map(|(id, v)| (id.to_owned(), v.to_owned()))
                .ok_or_else(|| format!("--set-live expects ID=VERSION, got `{s}`"))
        })
        .collect()
}

async fn evaluate(args: &Args) -> Result<(Report, EvalConfig), String> {
    let cfg: EvalConfig = read(&args.config, |t| {
        serde_yaml::from_str(t).map_err(|e| e.to_string())
    })?;
    let mut cases: Vec<Case> = read(&args.golden, |t| {
        serde_json::from_str(t).map_err(|e| e.to_string())
    })?;
    if let Some(name) = &args.case {
        cases.retain(|c| &c.name == name);
    }
    if cases.is_empty() {
        return Err("no golden cases selected".into());
    }
    let runs = args.runs.or(cfg.runs).unwrap_or(1).max(1);
    let overrides = parse_set_live(&args.set_live)?;
    let gw = Gateway::new(&args.gateway, Duration::from_secs(args.timeout_secs))?;

    let mut restore: Vec<(String, String)> = Vec::new();
    let admin_token = match (&args.admin_key, overrides.is_empty()) {
        (Some(key), false) => Some(gw.token(key).await?),
        _ => None,
    };
    let mut result = Ok(Vec::new());
    if let Some(admin) = &admin_token {
        for (id, version) in &overrides {
            let step = async {
                let previous = gw.live_version(admin, id).await?;
                gw.set_live(admin, id, version).await?;
                restore.push((id.clone(), previous));
                Ok::<_, String>(())
            };
            if let Err(e) = step.await {
                result = Err(e);
                break;
            }
        }
    }
    if result.is_ok() {
        result = run_cases(&gw, &cfg, &cases, runs).await;
    }
    if let Some(admin) = &admin_token {
        for (id, previous) in restore.iter().rev() {
            if let Err(e) = gw.set_live(admin, id, previous).await {
                eprintln!("warning: failed to restore {id} to {previous}: {e}");
            }
        }
    }
    let records = result?;

    let cases = report::summarize(&records);
    let totals = report::totals(&cases);
    let failures = report::threshold_failures(&totals, &cfg.thresholds);
    let mut prompts: Vec<String> = records
        .iter()
        .filter_map(|r| r.prompt_version.clone())
        .collect();
    prompts.sort();
    prompts.dedup();
    Ok((
        Report {
            gateway: args.gateway.clone(),
            runs_per_case: runs,
            prompts,
            cases,
            totals,
            failures,
            records,
        },
        cfg,
    ))
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    let (report, cfg) = match evaluate(&args).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };
    println!(
        "gateway {}  runs/case {}  prompts [{}]\n",
        report.gateway,
        report.runs_per_case,
        report.prompts.join(", ")
    );
    print!("{}", report::render(&report, &cfg.thresholds));
    if let Some(path) = &args.json_report {
        let written = serde_json::to_string_pretty(&report)
            .map_err(|e| e.to_string())
            .and_then(|s| std::fs::write(path, s).map_err(|e| e.to_string()));
        if let Err(e) = written {
            eprintln!("error: writing {}: {e}", path.display());
            return ExitCode::from(2);
        }
    }
    if report.failures.is_empty() {
        println!("\nPASS");
        ExitCode::SUCCESS
    } else {
        println!("\nFAIL: {}", report.failures.join(", "));
        ExitCode::from(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_live_specs_parse() {
        assert_eq!(
            parse_set_live(&["docs_agent=2.0.0".into()]).unwrap(),
            [("docs_agent".to_owned(), "2.0.0".to_owned())]
        );
        assert!(parse_set_live(&["docs_agent".into()]).is_err());
        assert!(parse_set_live(&["=1".into()]).is_err());
    }

    #[test]
    fn example_golden_sets_parse() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
        for ex in ["docs-assistant", "minimal"] {
            let golden = std::fs::read_to_string(root.join(ex).join("eval/golden.json")).unwrap();
            let cases: Vec<Case> = serde_json::from_str(&golden).unwrap();
            assert!(!cases.is_empty());
            let cfg = std::fs::read_to_string(root.join(ex).join("eval/eval.yaml")).unwrap();
            let cfg: EvalConfig = serde_yaml::from_str(&cfg).unwrap();
            for c in &cases {
                assert!(cfg.users.contains_key(&c.user), "{ex}/{}", c.name);
            }
        }
    }
}
