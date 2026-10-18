use std::sync::{Mutex, MutexGuard, OnceLock};

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Open,
    InProgress,
    Closed,
}

impl Status {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "open" => Ok(Self::Open),
            "in_progress" => Ok(Self::InProgress),
            "closed" => Ok(Self::Closed),
            _ => Err(format!(
                "invalid status '{s}': expected open, in_progress or closed"
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    Low,
    Medium,
    High,
}

impl Priority {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            _ => Err(format!(
                "invalid priority '{s}': expected low, medium or high"
            )),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Ticket {
    pub id: String,
    pub title: String,
    pub description: String,
    pub status: Status,
    pub priority: Priority,
    pub reporter: String,
    pub created_at: String,
}

struct Store {
    tickets: Vec<Ticket>,
    next: u32,
}

fn seed(
    id: u32,
    title: &str,
    description: &str,
    status: Status,
    priority: Priority,
    reporter: &str,
    created_at: &str,
) -> Ticket {
    Ticket {
        id: format!("TCK-{id}"),
        title: title.to_owned(),
        description: description.to_owned(),
        status,
        priority,
        reporter: reporter.to_owned(),
        created_at: created_at.to_owned(),
    }
}

fn store() -> MutexGuard<'static, Store> {
    static STORE: OnceLock<Mutex<Store>> = OnceLock::new();
    STORE
        .get_or_init(|| {
            Mutex::new(Store {
                tickets: vec![
                    seed(1001, "Consumer group rebalances every few minutes", "Members of group billing-workers keep rebalancing; heartbeats look healthy.", Status::Open, Priority::High, "alice", "2026-09-01T09:15:00Z"),
                    seed(1002, "Request to raise publish rate limit", "Need limits.publish_per_sec raised above 1000 for the orders topic.", Status::InProgress, Priority::Medium, "bob", "2026-09-03T14:02:00Z"),
                    seed(1003, "Docs: document mTLS client_auth values", "tls.client_auth accepted values are not listed in the docs.", Status::Closed, Priority::Low, "alice", "2026-08-20T11:40:00Z"),
                    seed(1004, "Dead-letter queue growing on payments topic", "payments.dlq has 40k messages after a downstream outage.", Status::Open, Priority::High, "carol", "2026-09-05T08:30:00Z"),
                ],
                next: 1005,
            })
        })
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub fn list(status: Option<&str>) -> Vec<Ticket> {
    let want = status.and_then(|s| Status::parse(s).ok());
    store()
        .tickets
        .iter()
        .filter(|t| want.is_none_or(|w| t.status == w))
        .cloned()
        .collect()
}

pub fn get(id: &str) -> Option<Ticket> {
    store().tickets.iter().find(|t| t.id == id).cloned()
}

pub fn create(title: String, description: String, priority: Priority, reporter: String) -> Ticket {
    let mut s = store();
    let ticket = Ticket {
        id: format!("TCK-{}", s.next),
        title,
        description,
        status: Status::Open,
        priority,
        reporter,
        created_at: now_rfc3339(),
    };
    s.next += 1;
    s.tickets.push(ticket.clone());
    ticket
}

fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs()) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Hinnant's civil-from-days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_by_status() {
        assert!(list(Some("open")).iter().all(|t| t.status == Status::Open));
        assert!(list(None).len() >= 4);
    }

    #[test]
    fn create_assigns_next_id_and_open_status() {
        let t = create("t".into(), "d".into(), Priority::Low, "dave".into());
        assert_eq!(t.status, Status::Open);
        assert_eq!(get(&t.id).map(|x| x.reporter), Some("dave".to_owned()));
    }

    #[test]
    fn timestamp_shape() {
        let s = now_rfc3339();
        assert_eq!(s.len(), 20);
        assert!(s.starts_with("20"));
    }

    #[test]
    fn rejects_bad_enums() {
        assert!(Status::parse("done").is_err());
        assert!(Priority::parse("urgent").is_err());
    }
}
