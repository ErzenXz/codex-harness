use anyhow::{Result, bail, ensure};
use std::path::{Component, Path, PathBuf};
use tokio::io::AsyncReadExt;

#[derive(Clone)]
pub struct Workspace {
    pub root: PathBuf,
}

impl Workspace {
    pub async fn new(path: &Path) -> Result<Self> {
        ensure!(path.is_absolute(), "Working directory must be absolute");
        let root = tokio::fs::canonicalize(path).await?;
        ensure!(root.is_dir(), "Working directory must be a directory");
        Ok(Self { root })
    }

    pub async fn resolve(&self, path: &str, create: bool) -> Result<PathBuf> {
        let input = Path::new(path);
        ensure!(
            !input
                .components()
                .any(|c| matches!(c, Component::ParentDir)),
            "Parent traversal is not allowed"
        );
        let candidate = if input.is_absolute() {
            input.to_path_buf()
        } else {
            self.root.join(input)
        };
        ensure!(
            candidate.starts_with(&self.root),
            "Path is outside the workspace"
        );
        let mut existing = candidate.as_path();
        while !tokio::fs::try_exists(existing).await? {
            ensure!(create, "Path does not exist");
            existing = existing
                .parent()
                .ok_or_else(|| anyhow::anyhow!("Invalid path"))?;
        }
        let real = tokio::fs::canonicalize(existing).await?;
        ensure!(
            real.starts_with(&self.root),
            "Symlink escapes the workspace"
        );
        let resolved = real.join(candidate.strip_prefix(existing)?);
        let relative = resolved.strip_prefix(&self.root)?;
        for part in relative.components() {
            let name = part.as_os_str().to_string_lossy();
            if name == ".git"
                || name == ".env"
                || name.starts_with(".env.")
                || matches!(name.as_ref(), ".ssh" | ".aws" | ".gnupg" | ".codex-harness")
            {
                bail!("Protected path; use a manually approved shell command if necessary");
            }
        }
        Ok(resolved)
    }

    pub async fn instructions(&self) -> Result<String> {
        let mut text = String::new();
        for name in ["AGENTS.md", "CLAUDE.md", ".harness/instructions.md"] {
            if let Ok(path) = self.resolve(name, false).await {
                let content = read_bounded(&path, 16384).await?;
                text.push_str(&format!("\n\nProject instructions ({name}):\n{content}"));
            }
        }
        Ok(text)
    }
}

pub async fn read_bounded(path: &Path, limit: usize) -> Result<String> {
    let mut options = tokio::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    let file = options.open(path).await?;
    ensure!(file.metadata().await?.is_file(), "Not a regular file");
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes).await?;
    ensure!(!bytes.contains(&0), "Binary file is not supported");
    let truncated = bytes.len() > limit;
    bytes.truncate(limit);
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if truncated {
        text.push_str("\n[truncated]");
    }
    Ok(text)
}

pub fn truncate(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[truncated]", &text[..end])
}
