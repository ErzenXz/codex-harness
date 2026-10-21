use anyhow::{Result, bail, ensure};
use std::path::{Component, Path, PathBuf};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio_util::sync::CancellationToken;

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
                || matches!(
                    name.as_ref(),
                    ".ssh" | ".aws" | ".gnupg" | ".codex-harness" | ".piko"
                )
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

/// A bounded page can continue beyond the first MiB without loading the file.
pub struct TextPage {
    pub content: String,
    pub next_offset_line: Option<usize>,
    pub line_truncated: bool,
}

pub async fn read_text_page(
    path: &Path,
    offset: usize,
    lines: usize,
    limit: usize,
    cancel: &CancellationToken,
) -> Result<TextPage> {
    let mut options = tokio::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    let file = options.open(path).await?;
    let metadata = file.metadata().await?;
    ensure!(metadata.is_file(), "Not a regular file");
    ensure_private_file(&metadata)?;
    let mut reader = BufReader::new(file);
    let mut content = String::new();
    let mut line = Vec::new();
    let mut output_bytes = 0usize;
    let mut line_bytes = 0usize;
    let mut number = 1usize;
    let mut returned = 0usize;
    loop {
        ensure!(!cancel.is_cancelled(), "Cancelled");
        let buffer = tokio::select! {
            _ = cancel.cancelled() => bail!("Cancelled"),
            result = reader.fill_buf() => result?,
        };
        if buffer.is_empty() {
            if !line.is_empty() {
                append_numbered_line(&mut content, &line, number, false)?;
            }
            ensure!(
                !content.is_empty() || offset == 1,
                "offset_line is beyond end of file"
            );
            return Ok(TextPage {
                content,
                next_offset_line: None,
                line_truncated: false,
            });
        }
        let newline = buffer.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(buffer.len(), |index| index + 1);
        if number >= offset {
            let segment = &buffer[..newline.unwrap_or(buffer.len())];
            let prefix = number.to_string().len() + 2 + 2 * usize::from(!content.is_empty());
            let available = limit.saturating_sub(output_bytes + prefix + line_bytes);
            let (fitting, escaped_bytes) = escaped_prefix(segment, available);
            if fitting < segment.len() {
                if content.is_empty() {
                    line.extend_from_slice(&segment[..fitting]);
                    append_numbered_line(&mut content, &line, number, true)?;
                    return Ok(TextPage {
                        content,
                        next_offset_line: Some(number + 1),
                        line_truncated: true,
                    });
                }
                return Ok(TextPage {
                    content,
                    next_offset_line: Some(number),
                    line_truncated: false,
                });
            }
            line.extend_from_slice(segment);
            line_bytes += escaped_bytes;
            if newline.is_some() {
                append_numbered_line(&mut content, &line, number, false)?;
                output_bytes += prefix + line_bytes;
                line.clear();
                line_bytes = 0;
                returned += 1;
            }
        }
        reader.consume(consumed);
        if newline.is_some() {
            number += 1;
            if returned >= lines {
                let more = tokio::select! {
                    _ = cancel.cancelled() => bail!("Cancelled"),
                    result = reader.fill_buf() => !result?.is_empty(),
                };
                return Ok(TextPage {
                    content,
                    next_offset_line: more.then_some(number),
                    line_truncated: false,
                });
            }
        }
    }
}

// Count the exact JSON string escaping cost without allocating another string.
// UTF-8 bytes stay unchanged; an incomplete final character is trimmed by the caller.
fn escaped_prefix(bytes: &[u8], limit: usize) -> (usize, usize) {
    let mut cost = 0;
    for (index, byte) in bytes.iter().enumerate() {
        let next = match byte {
            b'"' | b'\\' | b'\n' | b'\r' | b'\t' | 8 | 12 => 2,
            0..=31 => 6,
            _ => 1,
        };
        if cost + next > limit {
            return (index, cost);
        }
        cost += next;
    }
    (bytes.len(), cost)
}

fn append_numbered_line(
    output: &mut String,
    bytes: &[u8],
    number: usize,
    partial: bool,
) -> Result<()> {
    ensure!(!bytes.contains(&0), "Binary file is not supported");
    // A byte cap may split a UTF-8 character; omit just that incomplete suffix.
    // Reject actual encoding errors instead of inventing replacement characters.
    let text = match std::str::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) if partial && error.error_len().is_none() => {
            std::str::from_utf8(&bytes[..error.valid_up_to()])?
        }
        Err(error) => return Err(error.into()),
    };
    if !output.is_empty() {
        output.push('\n');
    }
    output.push_str(&format!(
        "{number}: {}",
        text.strip_suffix('\r').unwrap_or(text)
    ));
    Ok(())
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
    async fn text_pages_continue_beyond_first_megabyte_and_report_next_line() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("large.txt");
        let mut content = "padding padding padding\n".repeat(50000);
        content.push_str("last é\nfinal");
        tokio::fs::write(&path, content).await.unwrap();
        let cancel = CancellationToken::new();
        let page = read_text_page(&path, 50001, 1, 1024, &cancel)
            .await
            .unwrap();
        assert_eq!(page.content, "50001: last é");
        assert_eq!(page.next_offset_line, Some(50002));
        assert!(!page.line_truncated);
        let page = read_text_page(&path, 50002, 2, 1024, &cancel)
            .await
            .unwrap();
        assert_eq!(page.content, "50002: final");
        assert_eq!(page.next_offset_line, None);
        assert!(
            read_text_page(&path, 50003, 1, 1024, &cancel)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn page_boundary_inside_reader_buffer_never_skips_a_line() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file");
        let content = format!("{}\n{}\nlast", "a".repeat(8000), "b".repeat(1000));
        tokio::fs::write(&path, content).await.unwrap();
        let cancel = CancellationToken::new();
        let page = read_text_page(&path, 1, 100, 8500, &cancel).await.unwrap();
        assert_eq!(page.content, format!("1: {}", "a".repeat(8000)));
        assert_eq!(page.next_offset_line, Some(2));
        assert!(!page.line_truncated);
        let page = read_text_page(&path, 2, 100, 8500, &cancel).await.unwrap();
        assert_eq!(page.content, format!("2: {}\n3: last", "b".repeat(1000)));
        assert_eq!(page.next_offset_line, None);
    }

    #[tokio::test]
    async fn text_page_budget_counts_json_escaping_and_preserves_complete_lines() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file");
        tokio::fs::write(&path, "\"\\\t\u{1}\n".repeat(30))
            .await
            .unwrap();
        let cancel = CancellationToken::new();
        let page = read_text_page(&path, 1, 100, 64, &cancel).await.unwrap();
        assert!(serde_json::to_string(&page.content).unwrap().len() - 2 <= 64);
        assert!(page.next_offset_line.is_some());
        assert!(!page.line_truncated);
        let next = page.next_offset_line.unwrap();
        let page = read_text_page(&path, next, 1, 64, &cancel).await.unwrap();
        assert!(page.content.starts_with(&format!("{next}: ")));
    }

    #[tokio::test]
    async fn text_pages_bound_long_lines_without_corrupting_utf8() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file");
        let cancel = CancellationToken::new();
        tokio::fs::write(&path, "é".repeat(10000)).await.unwrap();
        let page = read_text_page(&path, 1, 10, 8, &cancel).await.unwrap();
        assert!(page.content.len() <= 8);
        assert_eq!(page.content, "1: éé");
        assert!(page.line_truncated);
        assert_eq!(page.next_offset_line, Some(2));
        tokio::fs::write(&path, "first\r\nsecond\nthird")
            .await
            .unwrap();
        let page = read_text_page(&path, 1, 10, 12, &cancel).await.unwrap();
        assert_eq!(page.content, "1: first");
        assert_eq!(page.next_offset_line, Some(2));
        assert!(!page.line_truncated);
        cancel.cancel();
        assert!(read_text_page(&path, 1, 10, 1024, &cancel).await.is_err());
    }

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
        for name in [
            ".ENV",
            ".Env.local",
            ".GIT/config",
            ".SSH/id_rsa",
            ".PIKO/auth.json",
            ".CODEX-HARNESS/auth.json",
        ] {
            assert!(workspace.resolve(name, true).await.is_err(), "{name}");
        }
    }
}
