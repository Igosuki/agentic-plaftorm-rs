use std::sync::Arc;

use serde::Serialize;

use crate::{
    config::UnclassifiedMode,
    engine::{AgentEngine, Attribution, TokenUsage, TurnRequest},
    guard::extract_json,
    intent::{Classification, IntentClassifier, Scores},
    telemetry,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RouteMethod {
    Keywords,
    Llm,
    Explicit,
}

impl RouteMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Keywords => "keywords",
            Self::Llm => "llm",
            Self::Explicit => "explicit",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Routing {
    pub agent: String,
    pub method: RouteMethod,
    pub scores: Scores,
    /// Tokens spent by the fallback model; zero when keywords decided.
    #[serde(skip)]
    pub usage: TokenUsage,
}

#[derive(Debug, Clone)]
pub struct Unclassified {
    pub scores: Scores,
    /// Tokens spent by the fallback model before it declined.
    pub usage: TokenUsage,
}

pub struct Coordinator {
    app: String,
    classifier: IntentClassifier,
    mode: UnclassifiedMode,
    engine: Arc<dyn AgentEngine>,
    fallback_model: Option<String>,
    specialists: Vec<(String, String)>,
}

impl Coordinator {
    pub fn classifier(&self) -> &IntentClassifier {
        &self.classifier
    }

    /// `specialists` is `(name, description)`; `fallback_model` is a model spec, only consulted when `mode` is `Llm`.
    pub fn new(
        app: String,
        classifier: IntentClassifier,
        mode: UnclassifiedMode,
        engine: Arc<dyn AgentEngine>,
        fallback_model: Option<String>,
        specialists: Vec<(String, String)>,
    ) -> Self {
        Self {
            app,
            classifier,
            mode,
            engine,
            fallback_model,
            specialists,
        }
    }

    fn is_registered(&self, name: &str) -> bool {
        self.specialists.iter().any(|(n, _)| n == name)
    }

    pub async fn route(
        &self,
        input: &str,
        attribution: Attribution,
    ) -> Result<Routing, Unclassified> {
        let classification = self.classifier.classify(input);
        let scores = classification.scores().clone();
        if let Classification::Classified { agent, .. } = classification {
            if self.is_registered(&agent) {
                telemetry::record_intent(&self.app, "keywords", "classified");
                return Ok(Routing {
                    agent,
                    method: RouteMethod::Keywords,
                    scores,
                    usage: TokenUsage::default(),
                });
            }
            tracing::warn!(
                agent,
                "intent rules matched an agent that is not registered"
            );
        }
        telemetry::record_intent(&self.app, "keywords", "unclassified");

        let mut usage = TokenUsage::default();
        if self.mode == UnclassifiedMode::Llm {
            let (picked, used) = self.llm_pick(input, attribution).await;
            usage = used;
            if let Some(agent) = picked {
                telemetry::record_intent(&self.app, "llm", "classified");
                return Ok(Routing {
                    agent,
                    method: RouteMethod::Llm,
                    scores,
                    usage,
                });
            }
            telemetry::record_intent(&self.app, "llm", "unclassified");
        }
        Err(Unclassified { scores, usage })
    }

    async fn llm_pick(
        &self,
        input: &str,
        attribution: Attribution,
    ) -> (Option<String>, TokenUsage) {
        let Some(model) = self.fallback_model.as_ref() else {
            return (None, TokenUsage::default());
        };
        let catalog = self
            .specialists
            .iter()
            .map(|(n, d)| format!("- {n}: {d}"))
            .collect::<Vec<_>>()
            .join("\n");
        let preamble = format!(
            "You route user requests to one specialist agent.\nSpecialists:\n{catalog}\n\nRespond ONLY with a JSON object {{\"agent\": \"<name>\"}} naming the best specialist, or {{\"agent\": null}} if none fits."
        );
        let req = TurnRequest {
            model_spec: model.clone(),
            preamble,
            temperature: Some(0.0),
            max_turns: 1,
            tools: Vec::new(),
            caller: None,
            request_id: attribution.request_id.clone(),
            attribution: Attribution {
                agent: "coordinator".into(),
                ..attribution
            },
        };
        match self.engine.prompt(&req, input.to_owned()).await {
            Ok(r) => (pick_agent(&r.text, |n| self.is_registered(n)), r.usage),
            Err(e) => {
                tracing::warn!(error = %e, "coordinator model call failed");
                (None, e.usage())
            }
        }
    }
}

/// Parses `{"agent": string|null}` and keeps the answer only if `is_registered` accepts it.
pub fn pick_agent(reply: &str, is_registered: impl Fn(&str) -> bool) -> Option<String> {
    let v = extract_json(reply)?;
    let name = v.get("agent")?.as_str()?;
    is_registered(name).then(|| name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pick_validates_against_registry() {
        let reg = |n: &str| n == "docs_agent";
        assert_eq!(
            pick_agent(r#"{"agent":"docs_agent"}"#, reg).as_deref(),
            Some("docs_agent")
        );
        assert_eq!(
            pick_agent("```json\n{\"agent\": \"docs_agent\"}\n```", reg).as_deref(),
            Some("docs_agent")
        );
        assert_eq!(pick_agent(r#"{"agent":"made_up"}"#, reg), None);
        assert_eq!(pick_agent(r#"{"agent":null}"#, reg), None);
        assert_eq!(pick_agent("I think docs", reg), None);
    }
}
