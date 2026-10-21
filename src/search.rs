use crate::{
    tools::string,
    workspace::{Workspace, truncate},
};
use anyhow::{Context, Result, bail, ensure};
use globset::Glob;
use ignore::WalkBuilder;
use regex::Regex;
use serde_json::{Value, json};
use std::{path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncReadExt, BufReader},
    process::Command,
    sync::Semaphore,
};
use tokio_util::sync::CancellationToken;

pub async fn search(
    workspace: &Workspace,
    state: &Path,
    searches: &Semaphore,
    name: &str,
    args: &Value,
    limit: usize,
    cancel: &CancellationToken,
) -> Result<Value> {
    // Match the canonical workspace root even when callers use a platform alias
    // such as macOS /var for /private/var. Otherwise the private state exclusion
    // silently misses both ripgrep and the built-in walker.
    let canonical_state = tokio::fs::canonicalize(state).await.ok();
    let state = canonical_state.as_deref().unwrap_or(state);
    let pattern = string(args, "pattern")?.to_owned();
    ensure!(pattern.len() <= 4096, "Pattern too long");
    let _permit = tokio::select! {
        _ = cancel.cancelled() => bail!("Cancelled"),
        permit = searches.acquire() => permit?,
    };
    if let Some(result) = ripgrep(workspace, state, name, args, limit, cancel).await? {
        return Ok(result);
    }
    let root = workspace.root.clone();
    let state = state.to_path_buf();
    let filter = args["glob"].as_str().map(str::to_owned);
    let cancel = cancel.clone();
    let grep = name == "grep";
    tokio::task::spawn_blocking(move || {
        let glob = if grep { filter.as_deref().map(Glob::new).transpose()? } else { Some(Glob::new(&pattern)?) }.map(|g|g.compile_matcher());
        let regex = if grep { Some(Regex::new(&pattern)?) } else { None };
        let mut output = String::new();
        let mut count = 0;
        let mut capped = false;
        let mut bytes = Vec::new();
        let walker = WalkBuilder::new(&root).hidden(true).git_ignore(true).require_git(false).follow_links(false).build();
        for entry in walker.take(20001) {
            ensure!(!cancel.is_cancelled(), "Cancelled");
            let Ok(entry) = entry else { continue; };
            count += 1;
            if count > 20000 { capped = true; break; }
            if !entry.file_type().is_some_and(|f| f.is_file()) || entry.path().starts_with(&state) { continue; }
            if crate::workspace::ensure_private_file(&entry.metadata()?).is_err() { continue; }
            let relative = entry.path().strip_prefix(&root)?;
            if !glob.as_ref().is_none_or(|g|g.is_match(relative)) { continue; }
            if let Some(regex) = &regex {
                use std::io::Read;
                if entry.metadata()?.len() > 1024 * 1024 { continue; }
                bytes.clear();
                std::fs::File::open(entry.path())?.take(1024 * 1024).read_to_end(&mut bytes)?;
                if bytes.contains(&0) { continue; }
                for (i,line) in String::from_utf8_lossy(&bytes).lines().enumerate() {
                    if regex.is_match(line) { output.push_str(&format!("{}:{}: {}\n", relative.display(),i+1,truncate(line,1024))); }
                    if output.len() >= limit { capped = true; break; }
                }
            } else { output.push_str(&format!("{}\n",relative.display())); }
            if output.len() >= limit { capped = true; break; }
        }
        Ok(json!({"content":truncate(&output,limit),"truncated":capped,"scanned_entries":count.min(20000),"engine":"rust-regex"}))
    }).await?
}

async fn ripgrep(
    workspace: &Workspace,
    state: &Path,
    name: &str,
    args: &Value,
    limit: usize,
    cancel: &CancellationToken,
) -> Result<Option<Value>> {
    let mut command = Command::new("rg");
    command
        .current_dir(&workspace.root)
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
        .arg("--no-config")
        .arg("--no-require-git")
        .arg("--color=never")
        .arg("--threads=1");
    if let Ok(relative) = state.strip_prefix(&workspace.root) {
        let relative = relative.to_str().context("State path must be UTF-8")?;
        // Glob metacharacters in the configured state path must not weaken the exclusion.
        if relative.contains(['[', ']', '{', '}', '*', '?', '!', '\\']) || relative.is_empty() {
            return Ok(None);
        }
        command.args(["--glob", &format!("!/{relative}/**")]);
    }
    let glob = if name == "glob" {
        Some(Glob::new(string(args, "pattern")?)?)
    } else {
        args["glob"].as_str().map(Glob::new).transpose()?
    }
    .map(|g| g.compile_matcher());
    if name == "glob" {
        command.arg("--files");
    } else {
        command.args(["--json", "--max-filesize=128K", "--max-count=200"]);
        command.arg("-e").arg(string(args, "pattern")?);
    }
    command.arg("--").arg(".");
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("Could not start ripgrep"),
    };
    let mut stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
    let mut stderr = child.stderr.take().expect("piped stderr").take(4096);
    let errors = tokio::spawn(async move {
        let mut errors = Vec::new();
        let _ = stderr.read_to_end(&mut errors).await;
        errors
    });
    let mut output = String::new();
    let mut truncated = false;
    let read = async {
        while let Some(line) = crate::protocol::read_frame(&mut stdout).await? {
            let (path, text) = if name == "glob" {
                let path = std::str::from_utf8(&line)?
                    .trim_end_matches(['\r', '\n'])
                    .trim_start_matches("./")
                    .to_owned();
                (path.clone(), format!("{path}\n"))
            } else {
                let event: Value = serde_json::from_slice(&line)?;
                if event["type"] != "match" {
                    continue;
                }
                let Some(path) = event["data"]["path"]["text"].as_str() else {
                    continue;
                };
                let path = path.trim_start_matches("./").to_owned();
                let Some(text) = event["data"]["lines"]["text"].as_str() else {
                    continue;
                };
                let text = format!(
                    "{}:{}: {}\n",
                    path,
                    event["data"]["line_number"],
                    truncate(text.trim_end_matches(['\r', '\n']), 1024)
                );
                (path, text)
            };
            // Apply the user filter after gitignore, rather than overriding ignored paths with -g.
            let Ok(metadata) = tokio::fs::symlink_metadata(workspace.root.join(&path)).await else {
                continue;
            };
            if metadata.file_type().is_symlink()
                || crate::workspace::ensure_private_file(&metadata).is_err()
            {
                continue;
            }
            if glob.as_ref().is_some_and(|glob| !glob.is_match(&path)) {
                continue;
            }
            output.push_str(&text);
            if output.len() >= limit {
                truncated = true;
                break;
            }
        }
        Ok::<_, anyhow::Error>(())
    };
    let result = tokio::select! {
        _ = cancel.cancelled() => Err(anyhow::anyhow!("Cancelled")),
        _ = tokio::time::sleep(Duration::from_secs(15)) => Err(anyhow::anyhow!("Search timed out; narrow the glob")),
        result = read => result,
    };
    // stdout can reach its cap before rg exits; kill before waiting on the process.
    if result.is_err() || truncated {
        let _ = child.kill().await;
    }
    let status = child.wait().await?;
    result?;
    let errors = errors.await.unwrap_or_default();
    if !truncated {
        ensure!(
            status.success() || status.code() == Some(1),
            "ripgrep search failed: {}",
            String::from_utf8_lossy(&errors)
        );
    }
    Ok(Some(
        json!({"content":truncate(&output,limit),"truncated":truncated,"engine":"ripgrep"}),
    ))
}
