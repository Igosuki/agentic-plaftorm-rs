use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct Runbook {
    pub id: &'static str,
    pub title: &'static str,
    pub severity: &'static str,
    pub symptoms: &'static str,
    pub steps: &'static [&'static str],
}

pub static RUNBOOKS: &[Runbook] = &[
    Runbook {
        id: "rb-consumer-lag",
        title: "Consumer lag growing",
        severity: "medium",
        symptoms: "nimbus_consumer_lag_messages above 10000 for 5 minutes, or lag growing monotonically for 15 minutes. Escalate to high above 100000.",
        steps: &[
            "Run `nimbusctl lag --group <group>` and identify partitions with the highest lag.",
            "Check consumer pod health and restarts; a crash loop above 3 restarts in 10 minutes means roll back the last consumer deploy.",
            "If consumers are healthy, scale the consumer group up to at most the partition count (`nimbusctl scale <group> --replicas N`).",
            "If processing time per message exceeds 500 ms, inspect downstream dependencies for latency.",
            "Confirm lag decreases for 10 consecutive minutes before closing the incident.",
        ],
    },
    Runbook {
        id: "rb-disk-full",
        title: "Broker disk nearly full",
        severity: "high",
        symptoms: "Broker data volume above 85% used. At 95% the broker rejects writes.",
        steps: &[
            "Run `nimbusctl disk --broker <id>` to find the largest topics.",
            "Lower retention on non-critical topics: `nimbusctl topic set <topic> retention.hours=24`.",
            "Trigger log compaction: `nimbusctl compact --broker <id>`.",
            "If usage stays above 90% after 30 minutes, expand the volume by 50%.",
            "Never delete segment files by hand; use `nimbusctl` so offsets stay consistent.",
        ],
    },
    Runbook {
        id: "rb-broker-down",
        title: "Broker unreachable",
        severity: "high",
        symptoms: "Broker health check failing for 2 minutes, nimbus_brokers_online below the replication factor, or producers timing out.",
        steps: &[
            "Check `nimbusctl status` for the broker state and whether a leader election is in progress.",
            "Inspect the broker host: process alive, disk and memory, network reachability on port 9292.",
            "Restart the broker service once; if it does not rejoin within 5 minutes, page the storage on-call.",
            "Verify under-replicated partitions drop to 0 with `nimbusctl partitions --under-replicated`.",
            "Do not restart more than one broker at a time.",
        ],
    },
    Runbook {
        id: "rb-dlq-growth",
        title: "Dead-letter queue growing",
        severity: "low",
        symptoms: "nimbus_dlq_messages above 500 or growing by more than 100 per hour.",
        steps: &[
            "Sample recent entries with `nimbusctl dlq peek <topic> --limit 20` and group them by error reason.",
            "If the cause is a schema or poison message, fix the producer and keep the messages for replay.",
            "If the cause was transient, replay with `nimbusctl dlq replay <topic> --rate 100`.",
            "Purge only entries older than 7 days that were reviewed.",
        ],
    },
];

pub fn find(id: &str) -> Option<&'static Runbook> {
    RUNBOOKS.iter().find(|r| r.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_known_id() {
        assert_eq!(find("rb-disk-full").unwrap().severity, "high");
    }

    #[test]
    fn unknown_id_is_none() {
        assert!(find("rb-nope").is_none());
    }

    #[test]
    fn ids_unique_and_have_steps() {
        for (i, r) in RUNBOOKS.iter().enumerate() {
            assert!(!r.steps.is_empty());
            assert!(RUNBOOKS[i + 1..].iter().all(|o| o.id != r.id));
        }
    }
}
