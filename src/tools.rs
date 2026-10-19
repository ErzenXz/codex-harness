use crate::{
    agent::{AgentContext, run_agent},
    workspace::{read_bounded, truncate},
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
            "Read a workspace text file with 1-based numbered lines. Protected credential paths are denied.",
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
    if !read_only {
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
        "read_file" => {
            let path = ctx.workspace.resolve(string(args, "path")?, false).await?;
            ensure!(
                !path.starts_with(&ctx.config.state_dir),
                "Harness state is protected"
            );
            let content = read_bounded(&path, 1024 * 1024).await?;
            let offset = integer(args, "offset_line")?.max(1) as usize;
            let limit = integer(args, "limit")?.clamp(1, 1000) as usize;
            let output = content
                .lines()
                .enumerate()
                .skip(offset - 1)
                .take(limit)
                .map(|(i, line)| format!("{}: {line}", i + 1))
                .collect::<Vec<_>>()
                .join("\n");
            Ok(json!({"path":path,"content":truncate(&output,ctx.config.tool_output_bytes)}))
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
                true => Some(read_bounded(&path, 256 * 1024).await?),
                false => None,
            };
            if let Some(content) = &before {
                ensure!(
                    !content.ends_with("\n[truncated]"),
                    "File too large for safe editing"
                );
            }
            let after = if name == "write_file" {
                string(args, "content")?.to_owned()
            } else {
                let old = string(args, "old_text")?;
                ensure!(!old.is_empty(), "old_text cannot be empty");
                let before = before.as_ref().context("File does not exist")?;
                ensure!(
                    before.matches(old).count() == 1,
                    "old_text must match exactly once; read the file and retry"
                );
                before.replacen(old, string(args, "new_text")?, 1)
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
                Some(read_bounded(&path, 256 * 1024).await?)
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
        "spawn_agent" => spawn_agent(ctx, args).await,
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

async fn spawn_agent(ctx: &AgentContext, args: &Value) -> Result<Value> {
    ensure!(
        ctx.depth < ctx.config.max_subagent_depth,
        "Subagent depth limit reached"
    );
    let role = string(args, "role")?;
    ensure!(
        ["explore", "worker", "reviewer"].contains(&role),
        "Invalid subagent role"
    );
    let prompt = string(args, "prompt")?.to_owned();
    ensure!(prompt.len() <= 32768, "Subagent prompt too long");
    let background = args["background"].as_bool().context("Missing background")?;
    // Non-blocking acquisition avoids nested agents deadlocking the shared budget.
    let permit = ctx
        .agents
        .clone()
        .try_acquire_owned()
        .context("Subagent concurrency limit reached")?;
    let job = ctx
        .jobs
        .reserve("agent", ctx.agent_id.clone(), ctx.cancel.child_token())
        .await?;
    let mut child = ctx.clone();
    child.depth += 1;
    child.agent_id = Some(job.id.clone());
    child.cancel = job.cancel.clone();
    child.read_only = ctx.read_only || role != "worker";
    let task = job.clone();
    tokio::spawn(async move {
        let _permit = permit;
        let mut items = vec![json!({"role":"user","content":prompt})];
        let result = run_agent(&child, &mut items).await;
        let (status, output) = if child.cancel.is_cancelled() {
            ("cancelled", "Cancelled".into())
        } else {
            match result {
                Ok(result) => ("completed", result.text),
                Err(error) => ("failed", format!("{error:#}")),
            }
        };
        task.finish(status, output, None, child.config.tool_output_bytes)
            .await;
        let _ = child
            .peer
            .task_event(&child.session_id, &task.id, task.snapshot().await)
            .await;
    });
    ctx.peer
        .task_event(
            &ctx.session_id,
            &job.id,
            json!({"status":"running","kind":"agent","role":role,"parentId":ctx.agent_id}),
        )
        .await?;
    if background {
        return Ok(json!({"task_id":job.id,"status":"running"}));
    }
    loop {
        let snapshot = ctx
            .jobs
            .status(&job.id, ctx.agent_id.as_deref(), 30000, &ctx.cancel)
            .await?;
        if snapshot["status"] != "running" {
            return Ok(snapshot);
        }
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
