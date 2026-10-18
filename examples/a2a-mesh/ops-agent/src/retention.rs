//! Periodic eviction of finished tasks and their ADK sessions from the in-memory stores.

use std::{
    sync::{Arc, Weak},
    time::Duration,
};

use adk_server::a2a::v1::task_store::{InMemoryTaskStore, ListTasksParams, TaskStore};
use adk_session::{DeleteRequest, SessionService};
use chrono::{DateTime, Utc};

pub const DEFAULT_TTL_SECS: u64 = 900;
const MAX_INTERVAL: Duration = Duration::from_secs(60);

pub fn sweep_interval(ttl: Duration) -> Duration {
    (ttl / 4).min(MAX_INTERVAL).max(Duration::from_secs(1))
}

/// Deletes terminal tasks idle since before `now - ttl`, with the session adk-server keyed by their context.
/// Returns the number of tasks removed.
pub async fn sweep_once(
    store: &InMemoryTaskStore,
    sessions: &dyn SessionService,
    app_name: &str,
    ttl: Duration,
    now: DateTime<Utc>,
) -> usize {
    let Ok(window) = chrono::TimeDelta::from_std(ttl) else {
        return 0;
    };
    let cutoff = now - window;
    let Ok(tasks) = store.list_tasks(ListTasksParams::default()).await else {
        return 0;
    };
    let mut removed = 0;
    for t in tasks
        .into_iter()
        .filter(|t| t.status.state.is_terminal() && t.updated_at < cutoff)
    {
        if store.delete_task(&t.id).await.is_ok() {
            removed += 1;
        }
        let _ = sessions
            .delete(DeleteRequest {
                app_name: app_name.to_owned(),
                user_id: format!("a2a-{}", t.context_id),
                session_id: t.context_id,
            })
            .await;
    }
    removed
}

/// Stops when the store is dropped.
pub fn spawn(
    store: &Arc<InMemoryTaskStore>,
    sessions: Arc<dyn SessionService>,
    app_name: &'static str,
    ttl: Duration,
) {
    let store: Weak<InMemoryTaskStore> = Arc::downgrade(store);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(sweep_interval(ttl));
        tick.tick().await;
        loop {
            tick.tick().await;
            let Some(store) = store.upgrade() else {
                return;
            };
            let n = sweep_once(&store, sessions.as_ref(), app_name, ttl, Utc::now()).await;
            if n > 0 {
                tracing::debug!(removed = n, "swept finished tasks");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use a2a_protocol_types::{TaskState, TaskStatus};
    use adk_server::a2a::v1::task_store::TaskStoreEntry;
    use adk_session::{CreateRequest, GetRequest, InMemorySessionService};

    use super::*;

    fn entry(id: &str, state: TaskState, updated_at: DateTime<Utc>) -> TaskStoreEntry {
        TaskStoreEntry {
            id: id.into(),
            context_id: format!("ctx-{id}"),
            status: TaskStatus::new(state),
            artifacts: Vec::new(),
            history: Vec::new(),
            metadata: HashMap::new(),
            push_configs: Vec::new(),
            created_at: updated_at,
            updated_at,
        }
    }

    async fn session(svc: &InMemorySessionService, ctx: &str) {
        svc.create(CreateRequest {
            app_name: "app".into(),
            user_id: format!("a2a-{ctx}"),
            session_id: Some(ctx.into()),
            state: HashMap::new(),
        })
        .await
        .unwrap();
    }

    async fn has_session(svc: &InMemorySessionService, ctx: &str) -> bool {
        svc.get(GetRequest {
            app_name: "app".into(),
            user_id: format!("a2a-{ctx}"),
            session_id: ctx.into(),
            num_recent_events: None,
            after: None,
        })
        .await
        .is_ok()
    }

    #[tokio::test]
    async fn sweeps_only_idle_terminal_tasks_and_their_sessions() {
        let now = Utc::now();
        let old = now - chrono::TimeDelta::seconds(120);
        let store = InMemoryTaskStore::new();
        let sessions = InMemorySessionService::new();
        for (id, state, at) in [
            ("done-old", TaskState::Completed, old),
            ("done-new", TaskState::Completed, now),
            ("busy-old", TaskState::Working, old),
        ] {
            store.create_task(entry(id, state, at)).await.unwrap();
            session(&sessions, &format!("ctx-{id}")).await;
        }

        let n = sweep_once(&store, &sessions, "app", Duration::from_secs(60), now).await;
        assert_eq!(n, 1);
        assert!(store.get_task("done-old").await.is_err());
        assert!(store.get_task("done-new").await.is_ok());
        assert!(store.get_task("busy-old").await.is_ok());
        assert!(!has_session(&sessions, "ctx-done-old").await);
        assert!(has_session(&sessions, "ctx-done-new").await);
        assert!(has_session(&sessions, "ctx-busy-old").await);
    }

    #[test]
    fn interval_is_a_quarter_of_the_ttl_between_one_and_sixty_seconds() {
        assert_eq!(
            sweep_interval(Duration::from_secs(900)),
            Duration::from_secs(60)
        );
        assert_eq!(
            sweep_interval(Duration::from_secs(80)),
            Duration::from_secs(20)
        );
        assert_eq!(
            sweep_interval(Duration::from_secs(1)),
            Duration::from_secs(1)
        );
    }
}
