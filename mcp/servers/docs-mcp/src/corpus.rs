use serde::Serialize;

pub struct Doc {
    pub id: &'static str,
    pub title: &'static str,
    pub content: &'static str,
}

pub static CORPUS: &[Doc] = &[
    Doc {
        id: "gs-001",
        title: "Getting Started with Nimbus Queue",
        content: include_str!("../corpus/gs-001.md"),
    },
    Doc {
        id: "cfg-001",
        title: "Configuration Reference",
        content: include_str!("../corpus/cfg-001.md"),
    },
    Doc {
        id: "auth-001",
        title: "Authentication",
        content: include_str!("../corpus/auth-001.md"),
    },
    Doc {
        id: "cg-001",
        title: "Consumer Groups",
        content: include_str!("../corpus/cg-001.md"),
    },
    Doc {
        id: "dlq-001",
        title: "Dead-Letter Queues",
        content: include_str!("../corpus/dlq-001.md"),
    },
    Doc {
        id: "rl-001",
        title: "Rate Limits",
        content: include_str!("../corpus/rl-001.md"),
    },
];

#[derive(Debug, Serialize, PartialEq)]
pub struct Hit {
    pub id: &'static str,
    pub title: &'static str,
    pub snippet: String,
    pub score: u32,
}

const SNIPPET_CHARS: usize = 200;

fn terms(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() > 1)
        .map(str::to_lowercase)
        .collect()
}

/// Term-overlap score: one point per query term in the body, three more if it is in the title.
fn score(doc: &Doc, query: &[String]) -> u32 {
    let body = terms(doc.content);
    let title = terms(doc.title);
    query
        .iter()
        .map(|q| u32::from(body.contains(q)) + 3 * u32::from(title.contains(q)))
        .sum()
}

fn snippet(doc: &Doc, query: &[String]) -> String {
    let line = doc
        .content
        .lines()
        .skip(1)
        .find(|l| terms(l).iter().any(|t| query.contains(t)))
        .or_else(|| doc.content.lines().nth(1))
        .unwrap_or_default();
    line.chars().take(SNIPPET_CHARS).collect()
}

pub fn search(corpus: &'static [Doc], query: &str, limit: usize) -> Vec<Hit> {
    let q = terms(query);
    let mut hits: Vec<Hit> = corpus
        .iter()
        .map(|d| (d, score(d, &q)))
        .filter(|(_, s)| *s > 0)
        .map(|(d, s)| Hit {
            id: d.id,
            title: d.title,
            snippet: snippet(d, &q),
            score: s,
        })
        .collect();
    hits.sort_by(|a, b| b.score.cmp(&a.score).then(a.id.cmp(b.id)));
    hits.truncate(limit);
    hits
}

pub fn find(id: &str) -> Option<&'static Doc> {
    CORPUS.iter().find(|d| d.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retention_query_ranks_config_first() {
        let hits = search(CORPUS, "what is the default retention hours", 3);
        assert_eq!(hits[0].id, "cfg-001");
        assert!(hits[0].snippet.contains("168"));
    }

    #[test]
    fn title_match_outweighs_body_match() {
        let hits = search(CORPUS, "dead-letter", 6);
        assert_eq!(hits[0].id, "dlq-001");
    }

    #[test]
    fn limit_and_no_match() {
        assert_eq!(search(CORPUS, "mtls api tokens consumer", 1).len(), 1);
        assert!(search(CORPUS, "zzzz qqqq", 5).is_empty());
        assert!(search(CORPUS, "", 5).is_empty());
    }

    #[test]
    fn ids_are_unique_and_findable() {
        for d in CORPUS {
            assert_eq!(find(d.id).map(|x| x.id), Some(d.id));
        }
        assert!(find("nope").is_none());
    }
}
