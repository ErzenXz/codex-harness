use crate::{config::Config, workspace::Workspace};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::VecDeque, path::PathBuf};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

const MAX_SESSION_BYTES: u64 = 16 * 1024 * 1024;

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
    pub turns: VecDeque<Turn>,
    pub dropped_turns: usize,
}

impl Session {
    pub fn new(workspace: &Workspace, config: &Config) -> Self {
        Self {
            version: 1,
            id: Uuid::new_v4(),
            cwd: workspace.root.clone(),
            model: config.model.clone(),
            mode: "code".into(),
            turns: VecDeque::new(),
            dropped_turns: 0,
        }
    }

    pub fn context(&mut self, budget: usize) -> Vec<Value> {
        // Drop only whole completed turns; never orphan a function-call output.
        let mut bytes = self.bytes();
        while !self.turns.is_empty() && bytes > budget {
            if let Some(turn) = self.turns.pop_front() {
                bytes =
                    bytes.saturating_sub(serde_json::to_vec(&turn).map_or(0, |bytes| bytes.len()));
            }
            self.dropped_turns += 1;
        }
        self.turns
            .iter()
            .flat_map(|t| t.items.iter().cloned())
            .collect()
    }

    pub fn bytes(&self) -> usize {
        self.turns
            .iter()
            .map(|t| serde_json::to_vec(t).map_or(0, |v| v.len()))
            .sum()
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
        let session: Session = serde_json::from_slice(&tokio::fs::read(path).await?)?;
        ensure!(
            session.version == 1 && session.id == id,
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
