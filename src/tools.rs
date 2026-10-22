use crate::{
    agent::AgentContext,
    workspace::{read_text_page, truncate},
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

fn function(name: &str, description: &str, properties: Value) -> Value {
    let required: Vec<_> = properties
        .as_object()
        .expect("properties")
        .keys()
        .cloned()
        .collect();
    json!({"type":"function","name":name,"description":description,"strict":true,
        "parameters":{"type":"object","properties":properties,"required":required,"additionalProperties":false}})
}

pub fn definitions(read_only: bool, can_spawn: bool) -> Vec<Value> {
    let mut tools = vec![
        function(
            "read_file",
            "Read a workspace UTF-8 text file with 1-based numbered lines. Follow next_offset_line to continue; line_truncated marks an oversized line. Protected credential paths are denied.",
            json!({"path":{"type":"string"},"offset_line":{"type":"integer","minimum":1},"limit":{"type":"integer","minimum":1,"maximum":1000}}),
        ),
        function(
            "glob",
            "Find files by workspace-relative glob; respects .gitignore and skips hidden files and symlinks. Results are capped.",
            json!({"pattern":{"type":"string"}}),
        ),
        function(
            "grep",
            "Search text files by regex, optionally filtered by glob. Bounded results, no hidden files or symlinks.",
            json!({"pattern":{"type":"string"},"glob":{"type":["string","null"]}}),
        ),
        function(
            "task_status",
            "Read a background task's bounded output and status. Wait at most 30 seconds per call.",
            json!({"task_id":{"type":"string"},"wait_ms":{"type":"integer","minimum":0,"maximum":30000}}),
        ),
        function(
            "task_cancel",
            "Cancel a background task and its descendants.",
            json!({"task_id":{"type":"string"}}),
        ),
        function(
            "update_plan",
            "Publish an actionable plan to the ACP client. Status: pending, in_progress, or completed.",
            json!({"entries":{"type":"array","maxItems":20,"items":{"type":"object","properties":{"content":{"type":"string"},"status":{"type":"string","enum":["pending","in_progress","completed"]}},"required":["content","status"],"additionalProperties":false}}}),
        ),
    ];
    tools.push(crate::image::definition());
    tools.extend([
        function("message_agent", "Send a bounded message to your running child agent. Delivered before its next model request; does not interrupt a request or tool already in progress.", json!({"task_id":{"type":"string"},"message":{"type":"string","maxLength":8192}})),
        function("followup_agent", "Resume a completed child agent with its bounded prior context. Use message_agent while it is running. Cancelled or evicted agents require spawning a new agent.", json!({"task_id":{"type":"string"},"message":{"type":"string","maxLength":8192}})),
    ]);
    if !read_only {
        tools.push(crate::patch::definition());
        tools.extend(crate::terminal::definitions());
        tools.extend([
            function("write_file", "Create or replace a workspace file after explicit client approval. Prefer edit_file for existing files.", json!({"path":{"type":"string"},"content":{"type":"string"}})),
            function("edit_file", "Replace exactly one matching string in an existing file after approval. Fails for absent or ambiguous matches.", json!({"path":{"type":"string"},"old_text":{"type":"string"},"new_text":{"type":"string"}})),
            function("shell", "Run a shell command in the workspace after explicit client approval. Output is bounded; background=true returns a monitorable task ID. Shell is NOT sandboxed.", json!({"command":{"type":"string"},"background":{"type":"boolean"},"timeout_secs":{"type":"integer","minimum":1,"maximum":3600}})),
        ]);
    }
    if can_spawn {
        tools.push(function("spawn_agent", "Delegate a focused task with an isolated context, sharing this workspace. Explore/reviewer agents are read-only. Background agents return a task ID; monitor and collect results. Never delegate the same task recursively.", json!({"prompt":{"type":"string"},"role":{"type":"string","enum":["explore","worker","reviewer"]},"background":{"type":"boolean"}})));
    }
    tools
}

pub async fn execute(ctx: &AgentContext, name: &str, args: &Value, call_id: &str) -> Result<Value> {
    ensure!(args.is_object(), "Tool arguments must be an object");
    ensure!(!ctx.cancel.is_cancelled(), "Cancelled");
    match name {
        "computer_use" => crate::computer::execute(ctx, args, call_id).await,
        "read_file" => {
            let path = ctx.workspace.resolve(string(args, "path")?, false).await?;
            ensure!(
                !path.starts_with(&ctx.config.state_dir),
                "Harness state is protected"
            );
            let offset = integer(args, "offset_line")?.max(1) as usize;
            let limit = integer(args, "limit")?.clamp(1, 1000) as usize;
            // The model receives serialized JSON, so reserve all metadata and
            // budget JSON escaping as well as the visible text itself.
            let overhead = json!({"path":path,"content":"","next_offset_line":usize::MAX,"truncated":false,"line_truncated":false}).to_string().len();
            let budget = ctx.config.tool_output_bytes.saturating_sub(overhead);
            ensure!(
                budget >= 32,
                "Path is too long for the configured tool output budget"
            );
            let page = read_text_page(&path, offset, limit, budget, &ctx.cancel).await?;
            Ok(
                json!({"path":path,"content":page.content,"next_offset_line":page.next_offset_line,"truncated":page.next_offset_line.is_some(),"line_truncated":page.line_truncated}),
            )
        }
        "glob" | "grep" => {
            crate::search::search(
                &ctx.workspace,
                &ctx.config.state_dir,
                &ctx.searches,
                name,
                args,
                ctx.config.tool_output_bytes,
                &ctx.cancel,
            )
            .await
        }
        "write_file" | "edit_file" => {
            ensure!(
                !ctx.read_only,
                "File changes are disabled in plan/read-only mode"
            );
            let path = ctx
                .workspace
                .resolve(string(args, "path")?, name == "write_file")
                .await?;
            ensure!(
                !path.starts_with(&ctx.config.state_dir),
                "Harness state is protected"
            );
            let before = match tokio::fs::try_exists(&path).await? {
                true => Some(read_editable_text(&path).await?),
                false => None,
            };
            let after = if name == "write_file" {
                string(args, "content")?.to_owned()
            } else {
                let before = before.as_ref().context("File does not exist")?;
                exact_edit(before, string(args, "old_text")?, string(args, "new_text")?)?
            };
            ensure!(after.len() <= 256 * 1024, "File write exceeds 256KiB limit");
            ctx.peer
                .permission(
                    &ctx.session_id,
                    call_id,
                    &format!("{name}: {}", path.display()),
                    "edit",
                    json!({"path":path,"operation":name,"oldBytes":before.as_ref().map_or(0,String::len),"newBytes":after.len()}),
                    &ctx.cancel,
                )
                .await?;
            // The human may have edited while approval was pending.
            let current = if tokio::fs::try_exists(&path).await? {
                Some(read_editable_text(&path).await?)
            } else {
                None
            };
            ensure!(
                current == before,
                "File changed while awaiting approval; read it again"
            );
            let checked = ctx
                .workspace
                .resolve(string(args, "path")?, name == "write_file")
                .await?;
            ensure!(checked == path, "Path changed while awaiting approval");
            ensure!(!ctx.cancel.is_cancelled(), "Cancelled");
            atomic_write(&path, &after).await?;
            Ok(json!({"path":path,"bytes":after.len(),"status":"written"}))
        }
        "shell" => {
            ensure!(
                !ctx.read_only,
                "Shell execution is disabled in plan/read-only mode"
            );
            let command = string(args, "command")?;
            ensure!(command.len() <= 16384, "Command too long");
            let timeout = integer(args, "timeout_secs")?.clamp(1, ctx.config.command_timeout_secs);
            ctx.peer
                .permission(
                    &ctx.session_id,
                    call_id,
                    command,
                    "execute",
                    json!({"command":command,"cwd":ctx.workspace.root}),
                    &ctx.cancel,
                )
                .await?;
            ctx.jobs
                .shell(
                    &ctx.workspace.root,
                    command,
                    timeout,
                    args["background"].as_bool().context("Missing background")?,
                    ctx.agent_id.clone(),
                    &ctx.cancel,
                    &ctx.peer,
                    &ctx.session_id,
                )
                .await
        }
        "task_status" => {
            ctx.jobs
                .status(
                    string(args, "task_id")?,
                    ctx.agent_id.as_deref(),
                    integer(args, "wait_ms")?,
                    &ctx.cancel,
                )
                .await
        }
        "task_cancel" => {
            let job = ctx.jobs.get(string(args, "task_id")?).await?;
            ensure!(
                ctx.agent_id.is_none() || job.parent == ctx.agent_id,
                "Task belongs to another agent"
            );
            job.cancel.cancel();
            Ok(json!({"task_id":job.id,"status":"cancelling"}))
        }
        "spawn_agent" => crate::agents::spawn(ctx, args).await,
        "message_agent" => crate::agents::message(ctx, args, false).await,
        "followup_agent" => crate::agents::message(ctx, args, true).await,
        "apply_patch" => crate::patch::execute(ctx, args, call_id).await,
        "view_image" => crate::image::execute(ctx, args, call_id).await,
        "web_search" | "web_fetch" => crate::web::execute(ctx, name, args, call_id).await,
        "browser" => crate::browser::execute(ctx, args, call_id).await,
        "terminal_start" | "terminal_write" | "terminal_resize" | "terminal_close" => {
            crate::terminal::execute(ctx, name, args, call_id).await
        }
        "update_plan" => {
            let entries = args["entries"].as_array().context("Missing entries")?;
            ensure!(entries.len() <= 20, "Plan exceeds 20 entries");
            let mut plan = Vec::new();
            for entry in entries {
                let status = string(entry, "status")?;
                ensure!(
                    ["pending", "in_progress", "completed"].contains(&status),
                    "Invalid plan status"
                );
                plan.push(json!({"content":truncate(string(entry,"content")?,1024),"priority":"medium","status":status}));
            }
            let mut update = json!({"sessionUpdate":"plan","entries":plan});
            if let Some(id) = &ctx.agent_id {
                update["_meta"] = json!({"agentId":id});
            }
            ctx.peer.update(&ctx.session_id, update).await?;
            Ok(json!({"status":"published"}))
        }
        _ => bail!("Unknown or disabled tool: {name}"),
    }
}

pub fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .with_context(|| format!("Missing string argument: {key}"))
}
fn integer(value: &Value, key: &str) -> Result<u64> {
    value[key]
        .as_u64()
        .with_context(|| format!("Missing nonnegative integer: {key}"))
}

async fn read_editable_text(path: &std::path::Path) -> Result<String> {
    use tokio::io::AsyncReadExt;
    let mut options = tokio::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    let file = options.open(path).await?;
    let metadata = file.metadata().await?;
    ensure!(metadata.is_file(), "Not a regular file");
    crate::workspace::ensure_private_file(&metadata)?;
    let mut bytes = Vec::new();
    file.take(256 * 1024 + 1).read_to_end(&mut bytes).await?;
    ensure!(
        bytes.len() <= 256 * 1024,
        "File too large for safe editing (256KiB limit)"
    );
    ensure!(!bytes.contains(&0), "Binary file is not supported");
    String::from_utf8(bytes).context(
        "File is not UTF-8; use an explicitly approved shell command to edit its original encoding",
    )
}

fn exact_edit(before: &str, old: &str, new: &str) -> Result<String> {
    ensure!(!old.is_empty(), "old_text cannot be empty");
    let start = before
        .find(old)
        .context("old_text was not found; read the file and retry")?;
    // Resume one character after the first start, rather than after its end:
    // "aa" is ambiguous in "aaa", even though str::matches reports it once.
    let next = start
        + before[start..]
            .chars()
            .next()
            .expect("nonempty match")
            .len_utf8();
    ensure!(
        !before[next..].contains(old),
        "old_text is ambiguous; include more surrounding text and retry"
    );
    let mut after = String::with_capacity(before.len() - old.len() + new.len());
    after.push_str(&before[..start]);
    after.push_str(new);
    after.push_str(&before[start + old.len()..]);
    Ok(after)
}

async fn atomic_write(path: &std::path::Path, text: &str) -> Result<()> {
    let parent = path.parent().context("Invalid target path")?;
    tokio::fs::create_dir_all(parent).await?;
    let temporary = parent.join(format!(".harness-{}.tmp", uuid::Uuid::new_v4()));
    let result: Result<()> = async {
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&temporary).await?;
        file.write_all(text.as_bytes()).await?;
        if let Ok(metadata) = tokio::fs::metadata(path).await {
            file.set_permissions(metadata.permissions()).await?;
        }
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(&temporary, path).await?;
        Ok(())
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(temporary).await;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_edits_reject_missing_overlapping_and_repeated_matches() {
        assert!(exact_edit("aaa", "aa", "b").is_err());
        assert!(exact_edit("ééé", "éé", "b").is_err());
        assert!(exact_edit("same same", "same", "b").is_err());
        assert!(exact_edit("text", "absent", "b").is_err());
        assert!(exact_edit("text", "", "b").is_err());
        assert_eq!(
            exact_edit("first\r\nunique\r\nlast", "unique", "changed").unwrap(),
            "first\r\nchanged\r\nlast"
        );
    }

    #[tokio::test]
    async fn editable_reads_reject_lossy_encoding_and_real_truncation() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file");
        tokio::fs::write(&path, b"before\xffafter").await.unwrap();
        assert!(read_editable_text(&path).await.is_err());
        tokio::fs::write(&path, "literal\n[truncated]")
            .await
            .unwrap();
        assert_eq!(
            read_editable_text(&path).await.unwrap(),
            "literal\n[truncated]"
        );
        tokio::fs::write(&path, vec![b'a'; 256 * 1024 + 1])
            .await
            .unwrap();
        assert!(read_editable_text(&path).await.is_err());
    }

    #[tokio::test]
    async fn atomic_writes_preserve_existing_permissions_and_clean_failed_temps() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file");
        tokio::fs::write(&path, "before").await.unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .await
                .unwrap();
        }
        atomic_write(&path, "after").await.unwrap();
        assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), "after");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                tokio::fs::metadata(&path)
                    .await
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o755
            );
        }
        let directory = root.path().join("directory");
        tokio::fs::create_dir(&directory).await.unwrap();
        assert!(atomic_write(&directory, "failure").await.is_err());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 2);
    }
}
