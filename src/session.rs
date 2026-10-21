use crate::{config::Config, workspace::Workspace};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::VecDeque, io::Write, path::PathBuf};
use tokio::io::AsyncReadExt;
use uuid::Uuid;

pub(crate) const MAX_SESSION_BYTES: u64 = 16 * 1024 * 1024;

// Count encoded JSON without allocating a second copy of the active context.
pub(crate) fn json_bytes(value: &(impl Serialize + ?Sized)) -> Result<usize> {
    struct Counter(usize);
    impl Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter(0);
    serde_json::to_writer(&mut counter, value)?;
    Ok(counter.0)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Turn {
    pub items: Vec<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Session {
    pub version: u32,
    pub id: Uuid,
    pub cwd: PathBuf,
    pub model: String,
    pub mode: String,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    pub turns: VecDeque<Turn>,
    pub dropped_turns: usize,
}

impl Session {
    pub fn new(workspace: &Workspace, config: &Config) -> Self {
        Self {
            version: 2,
            id: Uuid::new_v4(),
            cwd: workspace.root.clone(),
            model: config.model.clone(),
            mode: "code".into(),
            reasoning_effort: config.reasoning_effort.clone(),
            turns: VecDeque::new(),
            dropped_turns: 0,
        }
    }

    fn validate_identifiers(&self) -> Result<()> {
        ensure!(
            crate::config::model_identifier(&self.model),
            "Invalid persisted model identifier"
        );
        ensure!(
            self.reasoning_effort.as_ref().is_none_or(|effort| {
                !effort.is_empty()
                    && effort.len() <= 64
                    && effort
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            }),
            "Invalid persisted reasoning identifier"
        );
        Ok(())
    }

    /// Project whole turns into the model window without deleting saved history.
    pub fn context(&self, budget: usize) -> Vec<Value> {
        self.context_with_omitted(budget).0
    }

    pub fn context_with_omitted(&self, budget: usize) -> (Vec<Value>, usize) {
        let mut bytes = 2usize; // JSON array brackets and item separators.
        let mut keep = 0;
        for turn in self.turns.iter().rev() {
            let size = json_bytes(&turn.items).unwrap_or(usize::MAX);
            if size > budget.saturating_sub(bytes) {
                break;
            }
            bytes = bytes.saturating_add(size);
            keep += 1;
        }
        let omitted = self.turns.len() - keep;
        let items = self
            .turns
            .iter()
            .skip(omitted)
            .flat_map(|turn| turn.items.iter().cloned())
            .collect();
        (items, omitted)
    }

    pub fn bytes(&self) -> usize {
        self.turns.iter().map(|t| json_bytes(t).unwrap_or(0)).sum()
    }
}

#[derive(Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub async fn new(root: PathBuf) -> Result<Self> {
        tokio::fs::create_dir_all(&root).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).await?;
        }
        Ok(Self { root })
    }

    pub async fn save(&self, session: &Session) -> Result<()> {
        session.validate_identifiers()?;
        self.save_value(session.id, false, session, MAX_SESSION_BYTES as usize)
            .await?;
        // The committed conversation is durable before retiring its checkpoint.
        match tokio::fs::remove_file(self.active_path(session.id)).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }

    /// Save only this turn; completed history is not rewritten per tool step.
    pub async fn save_active(
        &self,
        session: &Session,
        active: &[Value],
        base_bytes: usize,
    ) -> Result<()> {
        session.validate_identifiers()?;
        // Recovery may need synthetic results for unanswered calls; reserve their
        // exact encoded size so a full session can still be loaded after a crash.
        let recovery = json_bytes(&crate::protocol::unanswered_outputs(active))?;
        let remaining = (MAX_SESSION_BYTES as usize)
            .saturating_sub(base_bytes)
            .saturating_sub(recovery)
            .saturating_sub(32);
        let snapshot = ActiveCheckpoint {
            session_id: session.id,
            base_turns: session.turns.len(),
            items: active,
        };
        self.save_value(session.id, true, &snapshot, remaining)
            .await
    }

    fn active_path(&self, id: Uuid) -> PathBuf {
        self.root.join(format!("{id}.active.json"))
    }

    async fn save_value(
        &self,
        id: Uuid,
        active: bool,
        value: &impl Serialize,
        limit: usize,
    ) -> Result<()> {
        // Encode once into a capped buffer; fail before copying an oversized payload.
        struct Capped {
            bytes: Vec<u8>,
            limit: usize,
        }
        impl Write for Capped {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
                    return Err(std::io::Error::other(
                        "Session reached its 16MiB persistence limit; start a new session. Saved history is preserved.",
                    ));
                }
                self.bytes.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut encoded = Capped {
            bytes: Vec::new(),
            limit,
        };
        serde_json::to_writer(&mut encoded, value)?;
        let target = if active {
            self.active_path(id)
        } else {
            self.root.join(format!("{id}.json"))
        };
        let root = self.root.clone();
        let temporary = root.join(format!(".{id}.{}.tmp", Uuid::new_v4()));
        // Batch filesystem work into one blocking dispatch so streams keep running.
        tokio::task::spawn_blocking(move || {
            struct Temporary(PathBuf);
            impl Drop for Temporary {
                fn drop(&mut self) {
                    let _ = std::fs::remove_file(&self.0);
                }
            }
            let cleanup = Temporary(temporary);
            let mut options = std::fs::OpenOptions::new();
            options.create_new(true).write(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&cleanup.0)?;
            file.write_all(&encoded.bytes)?;
            file.sync_all()?;
            drop(file);
            std::fs::rename(&cleanup.0, target)?;
            #[cfg(unix)]
            if active || root.join(format!("{id}.active.json")).try_exists()? {
                // Durable intent precedes host actions; a base commit must also
                // reach disk before retiring its active recovery checkpoint.
                std::fs::File::open(root)?.sync_all()?;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await
        .context("Session checkpoint writer interrupted")?
    }

    pub async fn load(&self, id: Uuid, workspace: &Workspace) -> Result<Session> {
        let path = self.root.join(format!("{id}.json"));
        ensure!(
            tokio::fs::metadata(&path).await?.len() <= MAX_SESSION_BYTES,
            "Session file too large"
        );
        let mut bytes = Vec::new();
        tokio::fs::File::open(path)
            .await?
            .take(MAX_SESSION_BYTES + 1)
            .read_to_end(&mut bytes)
            .await?;
        ensure!(
            bytes.len() as u64 <= MAX_SESSION_BYTES,
            "Session file too large"
        );
        let mut session: Session = serde_json::from_slice(&bytes)?;
        session.validate_identifiers()?;
        ensure!(
            matches!(session.version, 1 | 2) && session.id == id,
            "Invalid session format"
        );
        ensure!(
            session.cwd == workspace.root,
            "Session belongs to a different working directory"
        );
        ensure!(
            ["code", "plan"].contains(&session.mode.as_str()),
            "Invalid persisted mode"
        );
        let checkpoint = self.active_path(id);
        match tokio::fs::File::open(checkpoint).await {
            Ok(file) => {
                let mut bytes = Vec::new();
                file.take(MAX_SESSION_BYTES + 1)
                    .read_to_end(&mut bytes)
                    .await?;
                ensure!(
                    bytes.len() as u64 <= MAX_SESSION_BYTES,
                    "Active checkpoint is too large"
                );
                let active: LoadedCheckpoint = serde_json::from_slice(&bytes)?;
                ensure!(
                    active.session_id == id,
                    "Active checkpoint belongs to another session"
                );
                if active.base_turns == session.turns.len() {
                    session.turns.push_back(Turn {
                        items: active.items,
                    });
                    ensure!(
                        json_bytes(&session)? as u64 <= MAX_SESSION_BYTES,
                        "Recovered session is too large"
                    );
                } else {
                    // A crash after commit but before checkpoint cleanup leaves one stale file.
                    ensure!(
                        active.base_turns.checked_add(1) == Some(session.turns.len()),
                        "Active checkpoint does not match saved history"
                    );
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        // A process can stop between durable intent and a completed tool result.
        // Mark those calls interrupted; loading must never execute them again.
        for turn in &mut session.turns {
            crate::protocol::repair_calls(&mut turn.items);
        }
        Ok(session)
    }
}

#[derive(Serialize)]
struct ActiveCheckpoint<'a> {
    session_id: Uuid,
    base_turns: usize,
    items: &'a [Value],
}

#[derive(Deserialize)]
struct LoadedCheckpoint {
    session_id: Uuid,
    base_turns: usize,
    items: Vec<Value>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn persisted_identifiers_reject_oversized_control_and_invalid_reasoning() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(dir.path()).await.unwrap();
        let store = Store::new(dir.path().join("sessions")).await.unwrap();
        let mut session = Session::new(&workspace, &Config::default());
        let path = store.root.join(format!("{}.json", session.id));
        for model in [
            String::new(),
            "bad\nmodel".into(),
            "x".repeat(257),
            "é".repeat(129),
        ] {
            session.model = model;
            assert!(store.save(&session).await.is_err());
            tokio::fs::write(&path, serde_json::to_vec(&session).unwrap())
                .await
                .unwrap();
            assert!(store.load(session.id, &workspace).await.is_err());
        }
        session.model = "provider/custom:model".into();
        for effort in [
            String::new(),
            "bad\neffort".into(),
            "x".repeat(65),
            "bad effort".into(),
        ] {
            session.reasoning_effort = Some(effort);
            assert!(store.save(&session).await.is_err());
            tokio::fs::write(&path, serde_json::to_vec(&session).unwrap())
                .await
                .unwrap();
            assert!(store.load(session.id, &workspace).await.is_err());
        }
        session.model = "é".repeat(128);
        for version in [1, 2] {
            session.version = version;
            for effort in [None, Some("persistent".into()), Some("x".repeat(64))] {
                session.reasoning_effort = effort.clone();
                store.save(&session).await.unwrap();
                let loaded = store.load(session.id, &workspace).await.unwrap();
                assert_eq!(loaded.reasoning_effort, effort);
                assert_eq!(loaded.version, version);
            }
        }
    }

    #[test]
    fn counted_json_size_matches_encoding_with_escapes_and_unicode() {
        let value = json!({"message":"世界\n\t\"\\", "items":[null,true,-123,0.25]});
        assert_eq!(
            json_bytes(&value).unwrap(),
            serde_json::to_vec(&value).unwrap().len()
        );
        assert_eq!(
            json_bytes(&value["items"]).unwrap(),
            serde_json::to_vec(&value["items"]).unwrap().len()
        );
    }

    #[tokio::test]
    async fn context_projection_and_checkpoints_preserve_durable_history() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(dir.path()).await.unwrap();
        let store = Store::new(dir.path().join("sessions")).await.unwrap();
        let mut session = Session::new(&workspace, &Config::default());
        session.turns.push_back(Turn {
            items: vec![json!({"role":"user","content":"old history"})],
        });
        session.turns.push_back(Turn {
            items: vec![json!({"role":"user","content":"x".repeat(20000)})],
        });
        let (context, omitted) = session.context_with_omitted(100);
        assert!(context.is_empty());
        assert_eq!(omitted, 2);
        assert_eq!(session.turns.len(), 2);
        assert_eq!(session.dropped_turns, 0);
        let active = vec![
            json!({"type":"function_call","call_id":"pending","name":"write_file","arguments":"{}"}),
        ];
        store.save(&session).await.unwrap();
        let base_bytes = json_bytes(&session).unwrap();
        store
            .save_active(&session, &active, base_bytes)
            .await
            .unwrap();
        let loaded = store.load(session.id, &workspace).await.unwrap();
        assert_eq!(loaded.turns.len(), 3);
        assert_eq!(loaded.turns[0].items[0]["content"], "old history");
        assert_eq!(loaded.turns[2].items.len(), 2);
        assert_eq!(loaded.turns[2].items[1]["call_id"], "pending");
        assert!(
            loaded.turns[2].items[1]["output"]
                .as_str()
                .unwrap()
                .contains("interrupted")
        );
        let mut completed = active;
        completed.push(
            json!({"type":"function_call_output","call_id":"pending","output":"saved result"}),
        );
        store
            .save_active(&session, &completed, base_bytes)
            .await
            .unwrap();
        let loaded = store.load(session.id, &workspace).await.unwrap();
        assert_eq!(loaded.turns[2].items[1]["output"], "saved result");
    }

    #[tokio::test]
    async fn rejected_checkpoint_capacity_preserves_prior_base_and_active_files() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(dir.path()).await.unwrap();
        let store = Store::new(dir.path().join("sessions")).await.unwrap();
        let session = Session::new(&workspace, &Config::default());
        store.save(&session).await.unwrap();
        let active = vec![json!({"role":"user", "content":"durable intent"})];
        store
            .save_active(&session, &active, json_bytes(&session).unwrap())
            .await
            .unwrap();
        let base_path = store.root.join(format!("{}.json", session.id));
        let active_path = store.active_path(session.id);
        let base_before = tokio::fs::read(&base_path).await.unwrap();
        let active_before = tokio::fs::read(&active_path).await.unwrap();
        assert!(
            store
                .save_active(&session, &active, MAX_SESSION_BYTES as usize)
                .await
                .is_err()
        );
        // Exercise a base-write cap failure without allocating a 16MiB fixture.
        assert!(
            store
                .save_value(session.id, false, &session, 4)
                .await
                .is_err()
        );
        assert_eq!(tokio::fs::read(&base_path).await.unwrap(), base_before);
        assert_eq!(tokio::fs::read(&active_path).await.unwrap(), active_before);
        let recovered = store.load(session.id, &workspace).await.unwrap();
        assert_eq!(recovered.turns.len(), 1);
        assert_eq!(recovered.turns[0].items, active);
        assert_eq!(std::fs::read_dir(&store.root).unwrap().count(), 2);
    }

    #[tokio::test]
    async fn committed_history_ignores_stale_checkpoint_without_duplicate_turn() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(dir.path()).await.unwrap();
        let store = Store::new(dir.path().join("sessions")).await.unwrap();
        let mut session = Session::new(&workspace, &Config::default());
        store.save(&session).await.unwrap();
        let pending = vec![json!({"role":"user", "content":"stale checkpoint"})];
        store
            .save_active(&session, &pending, json_bytes(&session).unwrap())
            .await
            .unwrap();
        let stale = tokio::fs::read(store.active_path(session.id))
            .await
            .unwrap();
        let completed = vec![json!({"role":"assistant", "content":"committed answer"})];
        session.turns.push_back(Turn {
            items: completed.clone(),
        });
        store.save(&session).await.unwrap();
        assert!(!store.active_path(session.id).exists());
        // Recreate the crash window after base rename but before active cleanup.
        tokio::fs::write(store.active_path(session.id), stale)
            .await
            .unwrap();
        for _ in 0..2 {
            let recovered = store.load(session.id, &workspace).await.unwrap();
            assert_eq!(recovered.turns.len(), 1);
            assert_eq!(recovered.turns[0].items, completed);
        }
    }

    #[tokio::test]
    async fn checkpoint_identity_and_history_mismatches_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(dir.path()).await.unwrap();
        let store = Store::new(dir.path().join("sessions")).await.unwrap();
        let session = Session::new(&workspace, &Config::default());
        store.save(&session).await.unwrap();
        for checkpoint in [
            json!({"session_id":Uuid::new_v4(), "base_turns":0, "items":[]}),
            json!({"session_id":session.id, "base_turns":1, "items":[]}),
            json!({"session_id":session.id, "base_turns":usize::MAX, "items":[]}),
        ] {
            tokio::fs::write(store.active_path(session.id), checkpoint.to_string())
                .await
                .unwrap();
            assert!(store.load(session.id, &workspace).await.is_err());
        }
    }

    #[tokio::test]
    async fn active_checkpoint_is_private_and_failed_replacement_cleans_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(dir.path()).await.unwrap();
        let store = Store::new(dir.path().join("sessions")).await.unwrap();
        let session = Session::new(&workspace, &Config::default());
        let active = vec![json!({"role":"user", "content":"private intent"})];
        store
            .save_active(&session, &active, json_bytes(&session).unwrap())
            .await
            .unwrap();
        let active_path = store.active_path(session.id);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&active_path)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(&store.root).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        tokio::fs::remove_file(&active_path).await.unwrap();
        tokio::fs::create_dir(&active_path).await.unwrap();
        assert!(
            store
                .save_active(&session, &active, json_bytes(&session).unwrap())
                .await
                .is_err()
        );
        assert!(active_path.is_dir());
        let entries: Vec<_> = std::fs::read_dir(&store.root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0], active_path.file_name().unwrap());
    }
}
