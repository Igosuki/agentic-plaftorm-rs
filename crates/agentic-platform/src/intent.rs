use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AgentIntent {
    #[serde(default)]
    pub keywords: BTreeMap<String, u32>,
    #[serde(default)]
    pub phrases: BTreeMap<String, u32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IntentConfig {
    #[serde(default = "default_min_score")]
    pub min_score: u32,
    #[serde(default = "default_min_margin")]
    pub min_margin: u32,
    #[serde(default)]
    pub agents: BTreeMap<String, AgentIntent>,
}

fn default_min_score() -> u32 {
    2
}
fn default_min_margin() -> u32 {
    1
}

impl Default for IntentConfig {
    fn default() -> Self {
        Self {
            min_score: default_min_score(),
            min_margin: default_min_margin(),
            agents: BTreeMap::new(),
        }
    }
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

pub type Scores = BTreeMap<String, u32>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Classification {
    Classified { agent: String, scores: Scores },
    Unclassified { scores: Scores },
}

impl Classification {
    pub fn scores(&self) -> &Scores {
        match self {
            Self::Classified { scores, .. } | Self::Unclassified { scores } => scores,
        }
    }
}

#[derive(Debug, Clone)]
pub struct IntentClassifier {
    config: IntentConfig,
}

impl IntentClassifier {
    /// Keywords match whole words only, so they must consist of word characters; multi-word or punctuated terms belong in `phrases`.
    pub fn new(mut config: IntentConfig) -> Result<Self, String> {
        for (agent, a) in &config.agents {
            if let Some(bad) = a
                .keywords
                .keys()
                .find(|k| k.is_empty() || !k.chars().all(is_word_char))
            {
                return Err(format!(
                    "intents: agent `{agent}` keyword `{bad}` contains non-word characters and could never match a whole word; use `phrases` instead"
                ));
            }
        }
        for a in config.agents.values_mut() {
            a.keywords = a
                .keywords
                .iter()
                .map(|(k, v)| (k.to_lowercase(), *v))
                .collect();
            a.phrases = a
                .phrases
                .iter()
                .map(|(k, v)| (k.to_lowercase(), *v))
                .collect();
        }
        Ok(Self { config })
    }

    /// Drops intents of agents `keep` rejects so they cannot affect the top score or margin.
    pub fn retain_agents(&mut self, keep: impl Fn(&str) -> bool) {
        self.config.agents.retain(|name, _| keep(name));
    }

    /// Highest-weighted keywords of `agent`, for descriptive tags.
    pub fn top_keywords(&self, agent: &str, n: usize) -> Vec<String> {
        let Some(a) = self.config.agents.get(agent) else {
            return Vec::new();
        };
        let mut kw: Vec<(&String, &u32)> = a.keywords.iter().collect();
        kw.sort_by(|x, y| y.1.cmp(x.1).then(x.0.cmp(y.0)));
        kw.into_iter().take(n).map(|(k, _)| k.clone()).collect()
    }

    pub fn agent_names(&self) -> impl Iterator<Item = &str> {
        self.config.agents.keys().map(String::as_str)
    }

    pub fn scores(&self, input: &str) -> Scores {
        let lower = input.to_lowercase();
        let words: Vec<&str> = lower
            .split(|c: char| !is_word_char(c))
            .filter(|w| !w.is_empty())
            .collect();
        self.config
            .agents
            .iter()
            .map(|(name, intent)| {
                let kw: u32 = intent
                    .keywords
                    .iter()
                    .filter(|(k, _)| words.contains(&k.as_str()))
                    .map(|(_, w)| *w)
                    .sum();
                let ph: u32 = intent
                    .phrases
                    .iter()
                    .filter(|(p, _)| lower.contains(p.as_str()))
                    .map(|(_, w)| *w)
                    .sum();
                (name.clone(), kw + ph)
            })
            .collect()
    }

    pub fn classify(&self, input: &str) -> Classification {
        let scores = self.scores(input);
        let mut ranked: Vec<(&String, u32)> = scores.iter().map(|(k, v)| (k, *v)).collect();
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        let Some(&(top, top_score)) = ranked.first() else {
            return Classification::Unclassified { scores };
        };
        let runner_up = ranked.get(1).map_or(0, |r| r.1);
        if top_score >= self.config.min_score
            && top_score > 0
            && top_score - runner_up >= self.config.min_margin
        {
            Classification::Classified {
                agent: top.clone(),
                scores,
            }
        } else {
            Classification::Unclassified { scores }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classifier() -> IntentClassifier {
        let cfg: IntentConfig = serde_yaml::from_str(
            r#"
min_score: 2
min_margin: 1
agents:
  docs_agent:
    keywords: {docs: 2, documentation: 2, configure: 1, how: 1}
    phrases: {"how do i": 2}
  tickets_agent:
    keywords: {ticket: 2, tickets: 2, bug: 1}
"#,
        )
        .unwrap();
        IntentClassifier::new(cfg).unwrap()
    }

    fn agent(c: &Classification) -> Option<&str> {
        match c {
            Classification::Classified { agent, .. } => Some(agent),
            Classification::Unclassified { .. } => None,
        }
    }

    #[test]
    fn classifies_clear_intent() {
        let c = classifier().classify("How do I configure the docs?");
        assert_eq!(agent(&c), Some("docs_agent"));
        assert_eq!(c.scores()["docs_agent"], 6);
        assert_eq!(c.scores()["tickets_agent"], 0);
    }

    #[test]
    fn whole_word_only() {
        let c = classifier().classify("my documents are fine, subdocs too");
        assert_eq!(agent(&c), None);
        assert_eq!(c.scores()["docs_agent"], 0);
    }

    #[test]
    fn below_min_score_is_unclassified() {
        assert_eq!(agent(&classifier().classify("there is a bug")), None);
    }

    #[test]
    fn ambiguous_margin_is_unclassified() {
        let c = classifier().classify("docs ticket");
        assert_eq!(c.scores()["docs_agent"], 2);
        assert_eq!(c.scores()["tickets_agent"], 2);
        assert_eq!(agent(&c), None);
    }

    #[test]
    fn nothing_matches() {
        assert_eq!(agent(&classifier().classify("what is the weather")), None);
    }

    #[test]
    fn case_insensitive_keywords() {
        assert_eq!(
            agent(&classifier().classify("OPEN a TICKETS now")),
            Some("tickets_agent")
        );
    }

    #[test]
    fn non_word_keywords_rejected() {
        for kw in ["c++", "how do", "e-mail", "a.b", ""] {
            let mut cfg = IntentConfig::default();
            cfg.agents.insert(
                "a".into(),
                AgentIntent {
                    keywords: BTreeMap::from([(kw.to_owned(), 1)]),
                    phrases: BTreeMap::new(),
                },
            );
            let err = IntentClassifier::new(cfg).unwrap_err();
            assert!(err.contains("phrases"), "{err}");
        }
    }

    #[test]
    fn unregistered_agents_cannot_win() {
        let mut c = classifier();
        let before = c.classify("docs docs ticket ticket tickets");
        assert_eq!(agent(&before), Some("tickets_agent"));
        c.retain_agents(|n| n == "docs_agent");
        let after = c.classify("docs ticket tickets");
        assert_eq!(agent(&after), Some("docs_agent"));
        assert!(!after.scores().contains_key("tickets_agent"));
    }
}
