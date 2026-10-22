//! Exact-context patches with complete preflight and explicit partial-failure reporting.
use crate::{agent::AgentContext, workspace::Workspace};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const FILE_LIMIT: usize = 256 * 1024;
const TOTAL_LIMIT: usize = 1024 * 1024;
const FILES_LIMIT: usize = 32;

pub fn definition() -> Value {
    json!({"type":"function","name":"apply_patch","description":"Apply a bounded workspace patch after approval. Format: *** Begin Patch, *** Add File: path (+lines), *** Delete File: path, or *** Update File: path (optional *** Move to: path, @@ context hunks with space/context, -/remove, +/add); end with *** End Patch. Exact, unambiguous context is required. Optional *** End of File anchors a hunk. Maximum 32 operations, 256KiB/file, 1MiB total. Per-file writes are atomic; a multi-file I/O failure can leave explicitly reported partial changes.","strict":true,"parameters":{"type":"object","properties":{"patch":{"type":"string"}},"required":["patch"],"additionalProperties":false}})
}

#[derive(Debug)]
enum Operation {
    Add(String, String),
    Delete(String),
    Update(String, Option<String>, Vec<Hunk>),
}

#[derive(Debug)]
struct Hunk {
    anchor: Option<String>,
    old: Vec<String>,
    new: Vec<String>,
    eof: bool,
}

struct Change {
    name: String,
    path: PathBuf,
    before: Option<String>,
    after: Option<String>,
    permissions: Option<std::fs::Permissions>,
}

pub async fn execute(ctx: &AgentContext, args: &Value, call_id: &str) -> Result<Value> {
    ensure!(
        !ctx.read_only,
        "File changes are disabled in plan/read-only mode"
    );
    ensure!(!ctx.cancel.is_cancelled(), "Cancelled");
    let patch = args["patch"].as_str().context("Missing string: patch")?;
    let operations = parse(patch)?;
    let changes = prepare(&ctx.workspace, &ctx.config.state_dir, operations).await?;
    ensure!(!ctx.cancel.is_cancelled(), "Cancelled");
    let files: Vec<_> = changes.iter().map(|change| json!({"path":change.name,"oldBytes":change.before.as_ref().map_or(0,String::len),"newBytes":change.after.as_ref().map_or(0,String::len),"operation":if change.before.is_none(){"add"}else if change.after.is_none(){"delete"}else{"update"}})).collect();
    // Check the response budget before requesting approval or making changes.
    let response = json!({"status":"applied","files":files});
    ensure!(
        response.to_string().len() <= ctx.config.tool_output_bytes,
        "Patch summary exceeds tool output budget; split it into smaller patches"
    );
    ctx.peer
        .permission(
            &ctx.session_id,
            call_id,
            &format!("apply_patch: {} paths", changes.len()),
            "edit",
            json!({"files":files}),
            &ctx.cancel,
        )
        .await?;
    // Validate the entire snapshot again before the first mutation.
    for change in &changes {
        check(&ctx.workspace, &ctx.config.state_dir, change).await?;
    }
    let mut completed = Vec::new();
    for change in &changes {
        let result: Result<()> = async {
            ensure!(!ctx.cancel.is_cancelled(), "Cancelled");
            check(&ctx.workspace, &ctx.config.state_dir, change).await?;
            if let Some(after) = &change.after {
                atomic_write(
                    &change.path,
                    after,
                    change.before.is_none(),
                    change.permissions.clone(),
                )
                .await?;
            } else {
                tokio::fs::remove_file(&change.path).await?;
            }
            Ok(())
        }
        .await;
        if let Err(error) = result {
            bail!(
                "Patch stopped at {:?}: {error:#}. Already applied paths: {}. The current path may also have changed. Multi-file patches are not transactional; inspect these paths before retrying.",
                change.name,
                serde_json::to_string(&completed)?
            );
        }
        completed.push(&change.name);
    }
    Ok(response)
}

fn path_name(value: &str) -> Result<String> {
    ensure!(
        !value.is_empty() && !value.contains('\0'),
        "Patch path must be nonempty and contain no NUL"
    );
    ensure!(value.len() <= 4096, "Patch path exceeds 4096 bytes");
    Ok(value.to_owned())
}

fn parse(patch: &str) -> Result<Vec<Operation>> {
    ensure!(
        patch.len() <= TOTAL_LIMIT,
        "Patch exceeds 1MiB; split it into smaller patches"
    );
    ensure!(!patch.contains('\0'), "Binary patches are not supported");
    let lines: Vec<_> = patch.lines().collect();
    ensure!(
        lines.first() == Some(&"*** Begin Patch") && lines.last() == Some(&"*** End Patch"),
        "Expected *** Begin Patch and *** End Patch"
    );
    let mut at = 1;
    let mut operations = Vec::new();
    while at + 1 < lines.len() {
        ensure!(
            operations.len() < FILES_LIMIT,
            "Patch exceeds 32 operations"
        );
        let line = lines[at];
        at += 1;
        if let Some(name) = line.strip_prefix("*** Add File: ") {
            let mut text = String::new();
            while at + 1 < lines.len() && !lines[at].starts_with("*** ") {
                let added = lines[at]
                    .strip_prefix('+')
                    .context("Add File lines must start with +")?;
                text.push_str(added);
                text.push('\n');
                ensure!(text.len() <= FILE_LIMIT, "Added file exceeds 256KiB");
                at += 1;
            }
            operations.push(Operation::Add(path_name(name)?, text));
        } else if let Some(name) = line.strip_prefix("*** Delete File: ") {
            operations.push(Operation::Delete(path_name(name)?));
        } else if let Some(name) = line.strip_prefix("*** Update File: ") {
            let destination = if let Some(value) = lines
                .get(at)
                .and_then(|line| line.strip_prefix("*** Move to: "))
            {
                at += 1;
                Some(path_name(value)?)
            } else {
                None
            };
            let mut hunks = Vec::new();
            while at + 1 < lines.len() && !lines[at].starts_with("*** ") {
                let anchor = if lines[at] == "@@" {
                    None
                } else {
                    Some(
                        lines[at]
                            .strip_prefix("@@ ")
                            .context("Update hunks must start with @@ or @@ context")?
                            .to_owned(),
                    )
                };
                at += 1;
                let mut hunk = Hunk {
                    anchor,
                    old: Vec::new(),
                    new: Vec::new(),
                    eof: false,
                };
                let mut modified = false;
                while at + 1 < lines.len()
                    && !lines[at].starts_with("@@")
                    && !lines[at].starts_with("*** ")
                {
                    let line = lines[at];
                    at += 1;
                    match line.as_bytes().first() {
                        Some(b' ') => {
                            hunk.old.push(line[1..].to_owned());
                            hunk.new.push(line[1..].to_owned());
                        }
                        Some(b'-') => {
                            hunk.old.push(line[1..].to_owned());
                            modified = true;
                        }
                        Some(b'+') => {
                            hunk.new.push(line[1..].to_owned());
                            modified = true;
                        }
                        _ => bail!("Hunk lines must start with space, +, or -"),
                    }
                }
                if lines.get(at) == Some(&"*** End of File") {
                    hunk.eof = true;
                    at += 1;
                }
                ensure!(modified, "Hunk has no changes");
                hunks.push(hunk);
            }
            ensure!(!hunks.is_empty(), "Update File requires at least one hunk");
            operations.push(Operation::Update(path_name(name)?, destination, hunks));
        } else {
            bail!("Unknown patch directive: {line}");
        }
    }
    ensure!(!operations.is_empty(), "Patch contains no operations");
    Ok(operations)
}

fn apply(before: &str, hunks: &[Hunk]) -> Result<String> {
    let crlf = before.contains("\r\n") && !before.replace("\r\n", "").contains('\n');
    let original: Vec<_> = before.lines().map(str::to_owned).collect();
    let mut result = Vec::new();
    let mut cursor = 0;
    for hunk in hunks {
        let mut start = cursor;
        if let Some(anchor) = &hunk.anchor {
            let matches: Vec<_> = (cursor..original.len())
                .filter(|&i| original[i] == *anchor)
                .collect();
            ensure!(
                matches.len() == 1,
                "Hunk anchor is missing or ambiguous; include unique context"
            );
            start = matches[0] + 1;
        }
        let found = if hunk.old.is_empty() {
            // An anchor inserts immediately after its line; otherwise append.
            let position = if hunk.anchor.is_some() {
                start
            } else {
                original.len()
            };
            ensure!(
                !hunk.eof || position == original.len(),
                "End of File insertion must be at the end of the file"
            );
            position
        } else {
            let matches: Vec<_> = (start..=original.len())
                .filter(|&i| {
                    i + hunk.old.len() <= original.len()
                        && original[i..i + hunk.old.len()] == hunk.old
                        && (!hunk.eof || i + hunk.old.len() == original.len())
                })
                .collect();
            ensure!(
                matches.len() == 1,
                "Hunk context is missing or ambiguous; read the file and include unique exact context"
            );
            matches[0]
        };
        result.extend_from_slice(&original[cursor..found]);
        result.extend_from_slice(&hunk.new);
        cursor = found + hunk.old.len();
    }
    result.extend_from_slice(&original[cursor..]);
    let mut text = result.join(if crlf { "\r\n" } else { "\n" });
    if !result.is_empty() {
        text.push_str(if crlf { "\r\n" } else { "\n" });
    }
    ensure!(text.len() <= FILE_LIMIT, "Patched file exceeds 256KiB");
    Ok(text)
}

async fn resolve(workspace: &Workspace, state: &Path, name: &str) -> Result<PathBuf> {
    let path = workspace.resolve(name, true).await?;
    ensure!(!path.starts_with(state), "Harness state is protected");
    Ok(path)
}

async fn snapshot(path: &Path) -> Result<Option<String>> {
    // symlink_metadata distinguishes a dangling link from a missing add target.
    match tokio::fs::symlink_metadata(path).await {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
        Ok(metadata) => ensure!(
            !metadata.file_type().is_symlink(),
            "Patch target is a symlink"
        ),
    }
    let mut options = tokio::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    let file = options.open(path).await?;
    let metadata = file.metadata().await?;
    ensure!(metadata.is_file(), "Patch target is not a regular file");
    crate::workspace::ensure_private_file(&metadata)?;
    ensure!(
        metadata.len() <= FILE_LIMIT as u64,
        "File exceeds 256KiB editing limit"
    );
    let mut bytes = Vec::new();
    file.take(FILE_LIMIT as u64 + 1)
        .read_to_end(&mut bytes)
        .await?;
    ensure!(
        bytes.len() <= FILE_LIMIT && !bytes.contains(&0),
        "File exceeds 256KiB or contains binary data"
    );
    Ok(Some(
        String::from_utf8(bytes).context("Patch requires a UTF-8 file")?,
    ))
}

async fn prepare(
    workspace: &Workspace,
    state: &Path,
    operations: Vec<Operation>,
) -> Result<Vec<Change>> {
    let mut changes = Vec::new();
    let mut paths: HashSet<PathBuf> = HashSet::new();
    let mut total = 0;
    for operation in operations {
        let (name, after, before, path, destination) = match operation {
            Operation::Add(name, text) => {
                let path = resolve(workspace, state, &name).await?;
                ensure!(
                    snapshot(&path).await?.is_none(),
                    "Add target already exists: {name}"
                );
                (name, Some(text), None, path, None)
            }
            Operation::Delete(name) => {
                let path = resolve(workspace, state, &name).await?;
                let before = snapshot(&path)
                    .await?
                    .with_context(|| format!("Delete target missing: {name}"))?;
                (name, None, Some(before), path, None)
            }
            Operation::Update(name, destination, hunks) => {
                let path = resolve(workspace, state, &name).await?;
                let before = snapshot(&path)
                    .await?
                    .with_context(|| format!("Update target missing: {name}"))?;
                let after =
                    apply(&before, &hunks).with_context(|| format!("Cannot patch {name}"))?;
                (name, Some(after), Some(before), path, destination)
            }
        };
        total += before.as_ref().map_or(0, String::len) + after.as_ref().map_or(0, String::len);
        ensure!(
            total <= TOTAL_LIMIT,
            "Patch original and result contents exceed 1MiB total; split it"
        );
        ensure!(
            paths
                .iter()
                .all(|other| !path.starts_with(other) && !other.starts_with(&path)),
            "Duplicate or overlapping patch paths"
        );
        paths.insert(path.clone());
        if let Some(destination) = destination {
            let target = resolve(workspace, state, &destination).await?;
            ensure!(
                paths
                    .iter()
                    .all(|other| !target.starts_with(other) && !other.starts_with(&target)),
                "Duplicate or overlapping move target"
            );
            ensure!(
                snapshot(&target).await?.is_none(),
                "Move target already exists: {destination}"
            );
            paths.insert(target.clone());
            let permissions = Some(tokio::fs::metadata(&path).await?.permissions());
            // Create the destination before removing the source. Failure is reported honestly.
            changes.push(Change {
                name: destination,
                path: target,
                before: None,
                after,
                permissions,
            });
            changes.push(Change {
                name,
                path,
                before,
                after: None,
                permissions: None,
            });
        } else {
            changes.push(Change {
                name,
                path,
                before,
                after,
                permissions: None,
            });
        }
    }
    Ok(changes)
}

async fn check(workspace: &Workspace, state: &Path, change: &Change) -> Result<()> {
    ensure!(
        resolve(workspace, state, &change.name).await? == change.path,
        "Path changed since patch preparation: {}",
        change.name
    );
    ensure!(
        snapshot(&change.path).await? == change.before,
        "File changed since patch preparation: {}; read it again",
        change.name
    );
    Ok(())
}

async fn atomic_write(
    path: &Path,
    text: &str,
    create: bool,
    permissions: Option<std::fs::Permissions>,
) -> Result<()> {
    let parent = path.parent().context("Invalid target path")?;
    tokio::fs::create_dir_all(parent).await?;
    let temporary = parent.join(format!(".piko-{}.tmp", uuid::Uuid::new_v4()));
    let result: Result<()> = async {
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&temporary).await?;
        file.write_all(text.as_bytes()).await?;
        if let Some(permissions) = permissions {
            file.set_permissions(permissions).await?;
        } else if let Ok(metadata) = tokio::fs::metadata(path).await {
            file.set_permissions(metadata.permissions()).await?;
        }
        file.sync_all().await?;
        drop(file);
        if create {
            // Atomic no-clobber creation, including on Windows; unlike rename it cannot replace a new target.
            tokio::fs::hard_link(&temporary, path).await?;
            tokio::fs::remove_file(&temporary)
                .await
                .context("Target was created, but temporary hard-link cleanup failed")?;
        } else {
            tokio::fs::rename(&temporary, path).await?;
        }
        Ok(())
    }
    .await;
    let _ = tokio::fs::remove_file(&temporary).await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applies_multiple_hunks_with_anchor_eof_and_crlf() {
        let operations = parse("*** Begin Patch\n*** Update File: a\n@@\n first\n-old\n+new\n@@ section\n-last\n+end\n*** End of File\n*** End Patch\n").unwrap();
        let Operation::Update(_, _, hunks) = &operations[0] else {
            panic!()
        };
        assert_eq!(
            apply("first\r\nold\r\nsection\r\nlast\r\n", hunks).unwrap(),
            "first\r\nnew\r\nsection\r\nend\r\n"
        );
        assert!(apply("first\nold\nsection\nlast\nextra\n", hunks).is_err());
    }

    #[test]
    fn refuses_malformed_ambiguous_and_oversized_patches() {
        for patch in [
            "",
            "*** Begin Patch\n*** End Patch",
            "*** Begin Patch\n*** Update File: a\n@@\n context\n*** End Patch",
            "*** Begin Patch\n*** Add File: a\ninvalid\n*** End Patch",
            "*** Begin Patch\n*** Delete File: a\ntrailing\n*** End Patch",
        ] {
            assert!(parse(patch).is_err(), "{patch}");
        }
        let Operation::Update(_, _, hunks) =
            parse("*** Begin Patch\n*** Update File: a\n@@\n-same\n+new\n*** End Patch")
                .unwrap()
                .remove(0)
        else {
            panic!()
        };
        assert!(apply("same\nsame\n", &hunks).is_err());
        assert!(apply("missing\n", &hunks).is_err());
        assert!(parse(&"x".repeat(TOTAL_LIMIT + 1)).is_err());
    }

    #[test]
    fn additions_and_anchored_insertions_keep_order() {
        let Operation::Add(name, text) =
            parse("*** Begin Patch\n*** Add File: a\n+one\n+\n+two\n*** End Patch")
                .unwrap()
                .remove(0)
        else {
            panic!()
        };
        assert_eq!(name, "a");
        assert_eq!(text, "one\n\ntwo\n");
        let Operation::Update(_, _, hunks) =
            parse("*** Begin Patch\n*** Update File: a\n@@ anchor\n+inserted\n*** End Patch")
                .unwrap()
                .remove(0)
        else {
            panic!()
        };
        assert_eq!(
            apply("anchor\ntail\n", &hunks).unwrap(),
            "anchor\ninserted\ntail\n"
        );
    }

    #[test]
    fn eof_insertion_rejects_nonterminal_anchor() {
        let Operation::Update(_, _, hunks) = parse("*** Begin Patch\n*** Update File: a\n@@ anchor\n+inserted\n*** End of File\n*** End Patch").unwrap().remove(0) else { panic!() };
        assert!(apply("anchor\ntail\n", &hunks).is_err());
        assert_eq!(apply("anchor\n", &hunks).unwrap(), "anchor\ninserted\n");
    }

    #[tokio::test]
    async fn full_preflight_blocks_invalid_later_paths_and_stale_snapshots() {
        let root = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(root.path()).await.unwrap();
        let state = workspace.root.join("state");
        tokio::fs::write(root.path().join("old"), "old\n")
            .await
            .unwrap();
        let patch =
            "*** Begin Patch\n*** Add File: valid\n+hello\n*** Delete File: .env\n*** End Patch";
        assert!(
            prepare(&workspace, &state, parse(patch).unwrap())
                .await
                .is_err()
        );
        assert!(!root.path().join("valid").exists());
        let patch = "*** Begin Patch\n*** Update File: old\n*** Move to: nested/new\n@@\n-old\n+new\n*** End Patch";
        let changes = prepare(&workspace, &state, parse(patch).unwrap())
            .await
            .unwrap();
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].after.as_deref(), Some("new\n"));
        assert!(check(&workspace, &state, &changes[0]).await.is_ok());
        tokio::fs::write(root.path().join("old"), "human edit\n")
            .await
            .unwrap();
        assert!(check(&workspace, &state, &changes[1]).await.is_err());
        assert!(!root.path().join("nested").exists());
    }

    #[tokio::test]
    async fn rejects_duplicate_binary_and_existing_move_targets() {
        let root = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(root.path()).await.unwrap();
        let state = workspace.root.join("state");
        for (name, bytes) in [("a", b"old\n".as_slice()), ("binary", b"\xff\0".as_slice())] {
            tokio::fs::write(root.path().join(name), bytes)
                .await
                .unwrap();
        }
        for patch in [
            "*** Begin Patch\n*** Delete File: binary\n*** End Patch",
            "*** Begin Patch\n*** Delete File: a\n*** Add File: a\n+x\n*** End Patch",
            "*** Begin Patch\n*** Update File: a\n*** Move to: binary\n@@\n-old\n+new\n*** End Patch",
        ] {
            assert!(
                prepare(&workspace, &state, parse(patch).unwrap())
                    .await
                    .is_err()
            );
        }
        assert_eq!(
            tokio::fs::read(root.path().join("a")).await.unwrap(),
            b"old\n"
        );
    }

    #[tokio::test]
    async fn atomic_creation_never_overwrites_and_failed_temps_are_removed() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("file");
        atomic_write(&file, "original", true, None).await.unwrap();
        assert!(atomic_write(&file, "overwrite", true, None).await.is_err());
        assert_eq!(tokio::fs::read_to_string(&file).await.unwrap(), "original");
        atomic_write(&file, "updated", false, None).await.unwrap();
        assert_eq!(tokio::fs::read_to_string(file).await.unwrap(), "updated");
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }
}
