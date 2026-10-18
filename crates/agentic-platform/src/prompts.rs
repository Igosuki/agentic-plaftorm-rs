use std::{
    collections::{BTreeMap, HashMap},
    io::Write,
    path::{Path, PathBuf},
    sync::RwLock,
};

use serde::{Deserialize, Serialize};

use crate::{PlatformError, Result};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptVersion {
    pub version: String,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub temperature: Option<f64>,
    pub template: String,
}

#[derive(Debug, Clone, Deserialize)]
struct PromptFile {
    id: String,
    live: String,
    versions: Vec<PromptVersion>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedPrompt {
    pub id: String,
    pub version: String,
    pub template: String,
    pub model: Option<String>,
    pub temperature: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct VersionSummary {
    pub version: String,
    pub created_at: String,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PromptSummary {
    pub id: String,
    pub live: String,
    pub versions: Vec<VersionSummary>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PromptDetail {
    pub id: String,
    pub live: String,
    pub versions: Vec<PromptVersion>,
}

#[derive(Debug, thiserror::Error)]
pub enum PromptError {
    #[error("unknown prompt `{0}`")]
    UnknownPrompt(String),
    #[error("prompt `{id}` has no version `{version}`")]
    UnknownVersion { id: String, version: String },
    #[error("prompt `{0}` has no earlier live version to roll back to")]
    NoHistory(String),
    #[error("persisting prompt state: {0}")]
    Persist(String),
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct State {
    #[serde(default)]
    live: HashMap<String, String>,
    /// The YAML `live` each override was made against; an override is dropped when the YAML moves on.
    #[serde(default)]
    yaml_live: HashMap<String, String>,
    #[serde(default)]
    history: HashMap<String, Vec<String>>,
}

#[derive(Debug)]
pub struct PromptRegistry {
    prompts: BTreeMap<String, PromptFile>,
    state: RwLock<State>,
    state_file: PathBuf,
    /// Serialises writers across their persist `.await`, so the snapshot a writer builds on is never stale.
    writer: tokio::sync::Mutex<()>,
}

impl PromptRegistry {
    pub fn load(prompts_dir: &Path, state_file: &Path) -> Result<Self> {
        let mut prompts = BTreeMap::new();
        let entries = std::fs::read_dir(prompts_dir).map_err(|e| {
            PlatformError::Config(format!(
                "reading prompts dir {}: {e}",
                prompts_dir.display()
            ))
        })?;
        for entry in entries {
            let path = entry?.path();
            if !matches!(
                path.extension().and_then(|e| e.to_str()),
                Some("yaml" | "yml")
            ) {
                continue;
            }
            let file: PromptFile = serde_yaml::from_str(&std::fs::read_to_string(&path)?)
                .map_err(|e| PlatformError::Config(format!("{}: {e}", path.display())))?;
            if !file.versions.iter().any(|v| v.version == file.live) {
                return Err(PlatformError::Config(format!(
                    "{}: live version `{}` is not defined",
                    path.display(),
                    file.live
                )));
            }
            prompts.insert(file.id.clone(), file);
        }

        let mut state = read_state(state_file)?;
        let known = |id: &str, v: &str| {
            prompts
                .get(id)
                .is_some_and(|p| p.versions.iter().any(|x| x.version == v))
        };
        state.live.retain(|id, v| known(id, v));
        state.history.retain(|id, _| prompts.contains_key(id));
        for (id, h) in &mut state.history {
            h.retain(|v| known(id, v));
        }
        let mut stale = Vec::new();
        for id in state.live.keys().cloned().collect::<Vec<_>>() {
            let yaml = &prompts[&id].live;
            match state.yaml_live.get(&id) {
                Some(recorded) if recorded != yaml => stale.push(id),
                Some(_) => {}
                None => {
                    state.yaml_live.insert(id, yaml.clone());
                }
            }
        }
        for id in stale {
            tracing::warn!(
                prompt = %id,
                override_version = %state.live[&id],
                yaml_live = %prompts[&id].live,
                "prompt YAML `live` changed since the override was made; dropping the stale override"
            );
            state.live.remove(&id);
            state.history.remove(&id);
            state.yaml_live.remove(&id);
        }

        Ok(Self {
            prompts,
            state: RwLock::new(state),
            state_file: state_file.to_owned(),
            writer: tokio::sync::Mutex::new(()),
        })
    }

    fn live_version<'a>(&'a self, state: &'a State, file: &'a PromptFile) -> &'a str {
        state
            .live
            .get(&file.id)
            .map_or(file.live.as_str(), String::as_str)
    }

    pub fn contains(&self, id: &str) -> bool {
        self.prompts.contains_key(id)
    }

    pub fn resolve_live(&self, id: &str) -> Option<ResolvedPrompt> {
        let file = self.prompts.get(id)?;
        let state = self.state.read().unwrap_or_else(|e| e.into_inner());
        let live = self.live_version(&state, file);
        let v = file.versions.iter().find(|v| v.version == live)?;
        Some(ResolvedPrompt {
            id: file.id.clone(),
            version: v.version.clone(),
            template: v.template.clone(),
            model: v.model.clone(),
            temperature: v.temperature,
        })
    }

    pub fn list(&self) -> Vec<PromptSummary> {
        let state = self.state.read().unwrap_or_else(|e| e.into_inner());
        self.prompts
            .values()
            .map(|f| PromptSummary {
                id: f.id.clone(),
                live: self.live_version(&state, f).to_owned(),
                versions: f
                    .versions
                    .iter()
                    .map(|v| VersionSummary {
                        version: v.version.clone(),
                        created_at: v.created_at.clone(),
                        tags: v.tags.clone(),
                    })
                    .collect(),
            })
            .collect()
    }

    pub fn get(&self, id: &str) -> Option<PromptDetail> {
        let f = self.prompts.get(id)?;
        let state = self.state.read().unwrap_or_else(|e| e.into_inner());
        Some(PromptDetail {
            id: f.id.clone(),
            live: self.live_version(&state, f).to_owned(),
            versions: f.versions.clone(),
        })
    }

    pub async fn set_live(&self, id: &str, version: &str) -> std::result::Result<(), PromptError> {
        self.apply_live(id, version, true).await
    }

    /// Moves the live pointer without recording the previous version, so `rollback` history is untouched.
    pub async fn set_live_untracked(
        &self,
        id: &str,
        version: &str,
    ) -> std::result::Result<(), PromptError> {
        self.apply_live(id, version, false).await
    }

    async fn apply_live(
        &self,
        id: &str,
        version: &str,
        record_history: bool,
    ) -> std::result::Result<(), PromptError> {
        let file = self
            .prompts
            .get(id)
            .ok_or_else(|| PromptError::UnknownPrompt(id.to_owned()))?;
        if !file.versions.iter().any(|v| v.version == version) {
            return Err(PromptError::UnknownVersion {
                id: id.to_owned(),
                version: version.to_owned(),
            });
        }
        let _writer = self.writer.lock().await;
        let mut next = self.snapshot();
        let current = self.live_version(&next, file).to_owned();
        if current == version {
            return Ok(());
        }
        if record_history {
            next.history.entry(id.to_owned()).or_default().push(current);
        }
        next.live.insert(id.to_owned(), version.to_owned());
        next.yaml_live.insert(id.to_owned(), file.live.clone());
        self.commit(next).await
    }

    /// Returns the version that is live after the rollback.
    pub async fn rollback(&self, id: &str) -> std::result::Result<String, PromptError> {
        let file = self
            .prompts
            .get(id)
            .ok_or_else(|| PromptError::UnknownPrompt(id.to_owned()))?;
        let _writer = self.writer.lock().await;
        let mut next = self.snapshot();
        let previous = next
            .history
            .get_mut(id)
            .and_then(Vec::pop)
            .ok_or_else(|| PromptError::NoHistory(id.to_owned()))?;
        next.live.insert(id.to_owned(), previous.clone());
        next.yaml_live.insert(id.to_owned(), file.live.clone());
        self.commit(next).await?;
        Ok(previous)
    }

    fn snapshot(&self) -> State {
        self.state.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Persists off the async worker, then publishes; readers never see unpersisted state.
    async fn commit(&self, next: State) -> std::result::Result<(), PromptError> {
        let (path, to_write) = (self.state_file.clone(), next.clone());
        tokio::task::spawn_blocking(move || persist(&path, &to_write))
            .await
            .map_err(|e| PromptError::Persist(format!("persist task: {e}")))??;
        *self.state.write().unwrap_or_else(|e| e.into_inner()) = next;
        Ok(())
    }
}

/// An unreadable or corrupt state file is moved aside to `<file>.corrupt` and YAML defaults apply.
fn read_state(state_file: &Path) -> Result<State> {
    let bytes = match std::fs::read(state_file) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(State::default()),
        Err(e) => return Err(e.into()),
    };
    match serde_json::from_slice(&bytes) {
        Ok(state) => Ok(state),
        Err(e) => {
            let mut aside = state_file.as_os_str().to_owned();
            aside.push(".corrupt");
            let aside = PathBuf::from(aside);
            tracing::error!(
                file = %state_file.display(),
                moved_to = %aside.display(),
                error = %e,
                "prompt state file is empty or corrupt; starting from YAML defaults"
            );
            if let Err(re) = std::fs::rename(state_file, &aside) {
                tracing::error!(error = %re, "could not move the corrupt prompt state file aside");
            }
            Ok(State::default())
        }
    }
}

/// Write-to-temp, fsync, rename, fsync the directory: a crash leaves the old or the new file, never a torn one.
fn persist(state_file: &Path, state: &State) -> std::result::Result<(), PromptError> {
    let err =
        |e: &dyn std::fmt::Display| PromptError::Persist(format!("{}: {e}", state_file.display()));
    let dir = state_file
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir).map_err(|e| err(&e))?;
    let name = state_file
        .file_name()
        .map_or_else(|| "state".into(), |n| n.to_string_lossy().into_owned());
    let tmp = dir.join(format!(".{name}.{}.tmp", uuid::Uuid::new_v4()));
    let body = serde_json::to_vec_pretty(state).map_err(|e| err(&e))?;
    let write = || -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&body)?;
        f.sync_all()?;
        std::fs::rename(&tmp, state_file)?;
        std::fs::File::open(dir)?.sync_all()
    };
    write().map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        err(&e)
    })
}
#[cfg(test)]
mod tests {
    use super::*;

    const PROMPT: &str = r#"
id: docs_agent
live: 2.0.0
versions:
  - {version: 1.0.0, created_at: 2026-09-01T00:00:00Z, tags: [baseline], temperature: 0.2, template: "v1"}
  - {version: 2.0.0, created_at: 2026-09-02T00:00:00Z, model: "openai:x", template: "v2"}
  - {version: 3.0.0, template: "v3"}
"#;

    fn setup() -> (tempfile::TempDir, PromptRegistry) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("prompts")).unwrap();
        std::fs::write(dir.path().join("prompts/docs_agent.yaml"), PROMPT).unwrap();
        std::fs::write(dir.path().join("prompts/README.md"), "ignored").unwrap();
        let reg = load(&dir);
        (dir, reg)
    }

    fn load(dir: &tempfile::TempDir) -> PromptRegistry {
        PromptRegistry::load(
            &dir.path().join("prompts"),
            &dir.path().join("state/nested/s.json"),
        )
        .unwrap()
    }

    #[test]
    fn resolves_initial_live() {
        let (_d, reg) = setup();
        let p = reg.resolve_live("docs_agent").unwrap();
        assert_eq!((p.version.as_str(), p.template.as_str()), ("2.0.0", "v2"));
        assert_eq!(p.model.as_deref(), Some("openai:x"));
        assert!(reg.resolve_live("nope").is_none());
        assert_eq!(reg.list()[0].versions[0].tags, ["baseline"]);
    }

    #[tokio::test]
    async fn set_live_rollback_and_persist() {
        let (dir, reg) = setup();
        reg.set_live("docs_agent", "1.0.0").await.unwrap();
        reg.set_live("docs_agent", "3.0.0").await.unwrap();
        assert_eq!(reg.resolve_live("docs_agent").unwrap().version, "3.0.0");

        let reloaded = load(&dir);
        assert_eq!(
            reloaded.resolve_live("docs_agent").unwrap().version,
            "3.0.0"
        );
        assert_eq!(reloaded.rollback("docs_agent").await.unwrap(), "1.0.0");
        assert_eq!(reloaded.rollback("docs_agent").await.unwrap(), "2.0.0");
        assert!(matches!(
            reloaded.rollback("docs_agent").await,
            Err(PromptError::NoHistory(_))
        ));

        assert_eq!(
            load(&dir).resolve_live("docs_agent").unwrap().version,
            "2.0.0"
        );
    }

    #[tokio::test]
    async fn rejects_unknown() {
        let (_d, reg) = setup();
        assert!(matches!(
            reg.set_live("docs_agent", "9").await,
            Err(PromptError::UnknownVersion { .. })
        ));
        assert!(matches!(
            reg.set_live("x", "1.0.0").await,
            Err(PromptError::UnknownPrompt(_))
        ));
        assert!(matches!(
            reg.rollback("docs_agent").await,
            Err(PromptError::NoHistory(_))
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_resolution_during_switches() {
        let (_d, reg) = setup();
        let reg = std::sync::Arc::new(reg);
        let readers: Vec<_> = (0..4)
            .map(|_| {
                let r = reg.clone();
                std::thread::spawn(move || {
                    for _ in 0..200 {
                        let p = r.resolve_live("docs_agent").unwrap();
                        assert_eq!(p.template, format!("v{}", &p.version[..1]));
                    }
                })
            })
            .collect();
        for i in 0..20 {
            reg.set_live("docs_agent", if i % 2 == 0 { "1.0.0" } else { "3.0.0" })
                .await
                .unwrap();
        }
        for r in readers {
            r.join().unwrap();
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_writers_persist_what_they_publish() {
        let (dir, reg) = setup();
        let reg = std::sync::Arc::new(reg);
        let tasks: Vec<_> = (0..16)
            .map(|i| {
                let r = reg.clone();
                tokio::spawn(async move {
                    if i % 4 == 3 {
                        let _ = r.rollback("docs_agent").await;
                    } else {
                        let v = ["1.0.0", "3.0.0", "2.0.0"][i % 3];
                        r.set_live("docs_agent", v).await.unwrap();
                    }
                })
            })
            .collect();
        for t in tasks {
            t.await.unwrap();
        }
        let live = reg.resolve_live("docs_agent").unwrap().version;
        assert_eq!(load(&dir).resolve_live("docs_agent").unwrap().version, live);
        let leftovers: Vec<_> = std::fs::read_dir(dir.path().join("state/nested"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(leftovers, ["s.json"]);
    }

    #[tokio::test]
    async fn untracked_set_live_leaves_rollback_history_alone() {
        let (dir, reg) = setup();
        reg.set_live_untracked("docs_agent", "1.0.0").await.unwrap();
        reg.set_live_untracked("docs_agent", "2.0.0").await.unwrap();
        assert!(matches!(
            reg.rollback("docs_agent").await,
            Err(PromptError::NoHistory(_))
        ));
        assert_eq!(
            load(&dir).resolve_live("docs_agent").unwrap().version,
            "2.0.0"
        );

        reg.set_live("docs_agent", "3.0.0").await.unwrap();
        reg.set_live_untracked("docs_agent", "1.0.0").await.unwrap();
        assert_eq!(reg.rollback("docs_agent").await.unwrap(), "2.0.0");
    }

    #[test]
    fn empty_or_corrupt_state_falls_back_and_is_kept_aside() {
        for content in ["", "{not json"] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::create_dir(dir.path().join("prompts")).unwrap();
            std::fs::write(dir.path().join("prompts/docs_agent.yaml"), PROMPT).unwrap();
            let state = dir.path().join("s.json");
            std::fs::write(&state, content).unwrap();
            let reg = PromptRegistry::load(&dir.path().join("prompts"), &state).unwrap();
            assert_eq!(reg.resolve_live("docs_agent").unwrap().version, "2.0.0");
            assert!(!state.exists());
            assert_eq!(
                std::fs::read_to_string(dir.path().join("s.json.corrupt")).unwrap(),
                content
            );
        }
    }

    #[tokio::test]
    async fn override_is_dropped_when_yaml_live_changes() {
        let (dir, reg) = setup();
        reg.set_live("docs_agent", "3.0.0").await.unwrap();
        assert_eq!(
            load(&dir).resolve_live("docs_agent").unwrap().version,
            "3.0.0"
        );

        let moved = PROMPT.replace("live: 2.0.0", "live: 1.0.0");
        std::fs::write(dir.path().join("prompts/docs_agent.yaml"), moved).unwrap();
        let reloaded = load(&dir);
        assert_eq!(
            reloaded.resolve_live("docs_agent").unwrap().version,
            "1.0.0"
        );
        assert!(matches!(
            reloaded.rollback("docs_agent").await,
            Err(PromptError::NoHistory(_))
        ));
    }

    #[test]
    fn legacy_state_without_yaml_live_is_kept() {
        let (dir, _reg) = setup();
        std::fs::create_dir_all(dir.path().join("state/nested")).unwrap();
        std::fs::write(
            dir.path().join("state/nested/s.json"),
            r#"{"live":{"docs_agent":"3.0.0"},"history":{"docs_agent":["2.0.0"]}}"#,
        )
        .unwrap();
        assert_eq!(
            load(&dir).resolve_live("docs_agent").unwrap().version,
            "3.0.0"
        );
    }
}
