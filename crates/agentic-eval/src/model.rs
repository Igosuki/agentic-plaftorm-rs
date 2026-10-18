use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const UNCLASSIFIED: &str = "unclassified";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrajectoryMatch {
    Exact,
    #[default]
    InOrder,
    AnyOrder,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ResponseSpec {
    #[serde(default)]
    pub must_contain: Vec<String>,
    #[serde(default)]
    pub fields: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Case {
    pub name: String,
    pub user: String,
    pub input: String,
    pub expected_agent: String,
    #[serde(default)]
    pub expected_tools: Vec<String>,
    #[serde(default)]
    pub trajectory_match: TrajectoryMatch,
    #[serde(default)]
    pub response: ResponseSpec,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Thresholds {
    pub tool_trajectory_avg: Option<f64>,
    pub response_match_avg: Option<f64>,
    pub routing_accuracy: Option<f64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct EvalConfig {
    pub runs: Option<u32>,
    #[serde(default)]
    pub thresholds: Thresholds,
    #[serde(default)]
    pub users: BTreeMap<String, String>,
}

impl EvalConfig {
    /// A `user` that is not a key of `users` is used as the API key itself.
    pub fn api_key<'a>(&'a self, user: &'a str) -> &'a str {
        self.users.get(user).map_or(user, String::as_str)
    }
}
