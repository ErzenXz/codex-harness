use crate::{config::Config, workspace::Workspace};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::VecDeque, io::Write, path::PathBuf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;

const MAX_SESSION_BYTES: u64 = 16 * 1024 * 1024;

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

    pub fn context(&mut self, budget: usize) -> Vec<Value> {
        self.trim(budget);
        self.turns
            .iter()
            .flat_map(|t| t.items.iter().cloned())
            .collect()
    }

    pub fn trim(&mut self, budget: usize) {
        // Drop only whole completed turns; never orphan a function-call output.
        let mut bytes = 0;
        let mut keep = 0;
        // Count the retained suffix once; older discarded history needs no serialization.
        for turn in self.turns.iter().rev() {
            let size = json_bytes(turn).unwrap_or(0);
            if size > budget.saturating_sub(bytes) {
                break;
            }
            bytes += size;
            keep += 1;
        }
        let dropped = self.turns.len() - keep;
        self.turns.drain(..dropped);
        self.dropped_turns += dropped;
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
        let bytes = serde_json::to_vec(session)?;
        ensure!(
            bytes.len() as u64 <= MAX_SESSION_BYTES,
            "Session exceeds persistence limit"
        );
        let target = self.root.join(format!("{}.json", session.id));
        let temporary = self
            .root
            .join(format!(".{}.{}.tmp", session.id, Uuid::new_v4()));
        let result: Result<()> = async {
            let mut options = tokio::fs::OpenOptions::new();
            options.create_new(true).write(true);
            #[cfg(unix)]
            options.mode(0o600);
            let mut file = options.open(&temporary).await?;
            file.write_all(&bytes).await?;
            file.sync_all().await?;
            drop(file);
            tokio::fs::rename(&temporary, &target).await?;
            Ok(())
        }
        .await;
        if result.is_err() {
            let _ = tokio::fs::remove_file(&temporary).await;
        }
        result
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
        let session: Session = serde_json::from_slice(&bytes)?;
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
        Ok(session)
    }
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

    #[test]
    fn trim_evicts_complete_turns_and_keeps_exact_budget() {
        let first = Turn {
            items: vec![
                json!({"type":"function_call", "call_id":"one"}),
                json!({"type":"function_call_output", "call_id":"one", "output":"hello"}),
            ],
        };
        let last = Turn {
            items: vec![json!({"role":"user", "content":"keep me"})],
        };
        let budget = json_bytes(&last).unwrap();
        let mut session = Session {
            version: 1,
            id: Uuid::new_v4(),
            cwd: PathBuf::new(),
            model: String::new(),
            mode: "code".into(),
            reasoning_effort: None,
            turns: VecDeque::from([first, last]),
            dropped_turns: 0,
        };
        session.trim(budget);
        assert_eq!(session.turns.len(), 1);
        assert_eq!(session.dropped_turns, 1);
        assert_eq!(session.bytes(), budget);
        session.trim(budget);
        assert_eq!(session.dropped_turns, 1);
        assert_eq!(session.context(budget)[0]["content"], "keep me");
        session.trim(0);
        assert!(session.turns.is_empty());
        assert_eq!(session.dropped_turns, 2);
    }
}
