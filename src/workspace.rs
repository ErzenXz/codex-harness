use anyhow::{Result, bail, ensure};
use std::path::{Component, Path, PathBuf};
use tokio::io::AsyncReadExt;

#[derive(Clone)]
pub struct Workspace {
    pub root: PathBuf,
    protected: Vec<PathBuf>,
}

impl Workspace {
    pub async fn new(path: &Path) -> Result<Self> {
        ensure!(path.is_absolute(), "Working directory must be absolute");
        let root = tokio::fs::canonicalize(path).await?;
        ensure!(root.is_dir(), "Working directory must be a directory");
        Ok(Self {
            root,
            protected: Vec::new(),
        })
    }

    pub fn protect(&mut self, path: PathBuf) {
        // State paths may use /var while the workspace uses /private/var on
        // macOS, or a non-verbatim drive path on Windows. Resolve the nearest
        // existing ancestor even when the state directory is not created yet.
        let mut ancestor = path.as_path();
        let normalized = loop {
            if let Ok(real) = std::fs::canonicalize(ancestor) {
                break path
                    .strip_prefix(ancestor)
                    .map(|suffix| real.join(suffix))
                    .ok();
            }
            match ancestor.parent() {
                Some(parent) => ancestor = parent,
                None => break None,
            }
        };
        self.protected.push(normalized.unwrap_or(path));
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
        // Canonicalize before containment checks: Windows canonical paths have
        // a verbatim prefix (\\?\) that user-supplied absolute paths lack.
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
        let suffix = candidate.strip_prefix(existing)?;
        let resolved = if suffix.as_os_str().is_empty() {
            real
        } else {
            real.join(suffix)
        };
        if let Ok(metadata) = tokio::fs::metadata(&resolved).await {
            ensure_private_file(&metadata)?;
        }
        ensure!(
            !self.protected.iter().any(|path| resolved.starts_with(path)),
            "Harness state is protected"
        );
        let relative = resolved.strip_prefix(&self.root)?;
        for part in relative.components() {
            let name = part.as_os_str().to_string_lossy().to_ascii_lowercase();
            // NTFS alternate data streams can otherwise alias protected files,
            // for example `.env::$DATA`, without matching their base names.
            #[cfg(windows)]
            ensure!(
                !name.contains(':'),
                "Alternate data streams are not accessible through file tools"
            );
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
    let metadata = file.metadata().await?;
    ensure!(metadata.is_file(), "Not a regular file");
    ensure_private_file(&metadata)?;
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

pub fn ensure_private_file(metadata: &std::fs::Metadata) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            !metadata.is_file() || metadata.nlink() <= 1,
            "Hard-linked files are not accessible through file tools"
        );
    }
    #[cfg(not(unix))]
    let _ = metadata;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn absolute_paths_are_canonicalized_before_containment_checks() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(directory.path()).await.unwrap();
        let target = directory.path().join("new.txt");
        let resolved = workspace
            .resolve(target.to_str().unwrap(), true)
            .await
            .unwrap();
        assert_eq!(resolved, workspace.root.join("new.txt"));
        let outside = tempfile::tempdir().unwrap();
        assert!(
            workspace
                .resolve(outside.path().to_str().unwrap(), true)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn protected_paths_use_the_same_canonical_root_as_workspace() {
        let directory = tempfile::tempdir().unwrap();
        let mut workspace = Workspace::new(directory.path()).await.unwrap();
        workspace.protect(directory.path().join("future-state"));
        assert!(
            workspace
                .resolve("future-state/auth.json", true)
                .await
                .is_err()
        );
        assert!(workspace.resolve("source.rs", true).await.is_ok());
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn alternate_data_streams_cannot_alias_credential_files() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(directory.path()).await.unwrap();
        for name in [".env::$DATA", ".env:stream", "ordinary.txt:stream"] {
            assert!(workspace.resolve(name, true).await.is_err(), "{name}");
        }
    }

    #[tokio::test]
    async fn credential_names_are_protected_regardless_of_case() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(directory.path()).await.unwrap();
        for name in [".ENV", ".Env.local", ".GIT/config", ".SSH/id_rsa"] {
            assert!(workspace.resolve(name, true).await.is_err(), "{name}");
        }
    }
}
