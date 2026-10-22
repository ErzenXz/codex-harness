//! Bounded native PTYs (Unix PTY / Windows ConPTY), integrated with background jobs.
use crate::{
    agent::AgentContext,
    jobs::{EXECUTION_ENV, Job, Jobs, ProcessGroup},
    protocol::Peer,
    tools::string,
};
use anyhow::{Context, Result, bail, ensure};
#[cfg(windows)]
use base64::{Engine, engine::general_purpose::STANDARD};
use portable_pty::{CommandBuilder, MasterPty, PtySize};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

const MAX_INPUT: usize = 16 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(2);

pub struct Terminal {
    input: std::sync::mpsc::SyncSender<Input>,
    resize: mpsc::Sender<Resize>,
}
struct Input {
    data: Vec<u8>,
    response: oneshot::Sender<Result<()>>,
}
struct Resize {
    size: PtySize,
    response: oneshot::Sender<Result<()>>,
}

pub fn definitions() -> Vec<Value> {
    let function = |name: &str, description: &str, properties: Value| {
        let required: Vec<_> = properties
            .as_object()
            .expect("properties")
            .keys()
            .cloned()
            .collect();
        json!({"type":"function","name":name,"description":description,"strict":true,"parameters":{"type":"object","properties":properties,"required":required,"additionalProperties":false}})
    };
    vec![
        function(
            "terminal_start",
            "Start a command in a genuine interactive terminal after approval (Unix sh; Windows PowerShell). Returns a task_id; use task_status for bounded output. Execution is not sandboxed. Lifetime is limited by timeout_secs.",
            json!({"command":{"type":"string"},"timeout_secs":{"type":"integer","minimum":1},"rows":{"type":"integer","minimum":1,"maximum":500},"cols":{"type":"integer","minimum":1,"maximum":500}}),
        ),
        function(
            "terminal_write",
            "Send at most 16KiB of terminal input after explicit approval; input can execute commands. Include a newline to submit a command. Use task_status to read output.",
            json!({"task_id":{"type":"string"},"data":{"type":"string","maxLength":16384}}),
        ),
        function(
            "terminal_resize",
            "Resize your interactive terminal to 1..500 rows and columns.",
            json!({"task_id":{"type":"string"},"rows":{"type":"integer","minimum":1,"maximum":500},"cols":{"type":"integer","minimum":1,"maximum":500}}),
        ),
        function(
            "terminal_close",
            "Close your interactive terminal and cancel its process tree.",
            json!({"task_id":{"type":"string"}}),
        ),
    ]
}

pub async fn execute(ctx: &AgentContext, name: &str, args: &Value, call_id: &str) -> Result<Value> {
    ensure!(
        !ctx.read_only,
        "Interactive terminals are disabled in plan/read-only mode"
    );
    ensure!(!ctx.cancel.is_cancelled(), "Cancelled");
    if name == "terminal_start" {
        let command = string(args, "command")?;
        ensure!(
            !command.trim().is_empty() && command.len() <= MAX_INPUT && !command.contains('\0'),
            "Invalid terminal command"
        );
        let timeout = args["timeout_secs"]
            .as_u64()
            .context("Missing terminal timeout")?
            .clamp(1, ctx.config.command_timeout_secs);
        let size = size(args)?;
        ctx.peer.permission(&ctx.session_id, call_id, "Start interactive terminal", "execute", json!({"command":command,"cwd":ctx.workspace.root,"timeout_secs":timeout,"rows":size.rows,"cols":size.cols}), &ctx.cancel).await?;
        return start(
            &ctx.jobs,
            &ctx.workspace.root,
            command,
            timeout,
            size,
            ctx.agent_id.clone(),
            &ctx.cancel,
            &ctx.peer,
            &ctx.session_id,
            ctx.config.tool_output_bytes,
        )
        .await;
    }
    let job = ctx.jobs.get(string(args, "task_id")?).await?;
    ensure!(
        ctx.agent_id.is_none() || job.parent == ctx.agent_id,
        "Terminal belongs to another agent"
    );
    ensure!(
        job.kind == "terminal",
        "Task is not an interactive terminal"
    );
    if name == "terminal_close" {
        job.cancel.cancel();
        return match tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let snapshot = ctx
                    .jobs
                    .status(&job.id, ctx.agent_id.as_deref(), 3000, &ctx.cancel)
                    .await?;
                if snapshot["status"] != "running" {
                    return Ok(snapshot);
                }
            }
        })
        .await
        {
            Ok(result) => result,
            Err(_) => Ok(json!({"task_id":job.id,"status":"cancelling"})),
        };
    }
    let terminal = job
        .terminal
        .lock()
        .await
        .clone()
        .context("Terminal has closed")?;
    ensure!(
        job.state.lock().await.status == "running" && !job.cancel.is_cancelled(),
        "Terminal has closed"
    );
    match name {
        "terminal_write" => {
            let data = string(args, "data")?;
            ensure!(
                !data.is_empty() && data.len() <= MAX_INPUT,
                "Terminal input must be 1..=16384 bytes"
            );
            ctx.peer
                .permission(
                    &ctx.session_id,
                    call_id,
                    "Write interactive terminal input",
                    "execute",
                    json!({"task_id":job.id,"data":data}),
                    &ctx.cancel,
                )
                .await?;
            ensure!(
                !ctx.cancel.is_cancelled() && !job.cancel.is_cancelled(),
                "Terminal cancelled"
            );
            let (tx, rx) = oneshot::channel();
            let result = tokio::time::timeout(IO_TIMEOUT, async {
                queue_input(
                    &terminal.input,
                    Input {
                        data: data.as_bytes().to_vec(),
                        response: tx,
                    },
                )
                .await?;
                rx.await.context("Terminal input closed")?
            });
            tokio::select! {
                _ = ctx.cancel.cancelled() => { job.cancel.cancel(); bail!("Cancelled"); },
                result = result => match result { Ok(result) => result?, Err(_) => { job.cancel.cancel(); bail!("Terminal input timed out; terminal cancelled"); } },
            }
            Ok(json!({"task_id":job.id,"bytes_written":data.len(),"status":"running"}))
        }
        "terminal_resize" => {
            let size = size(args)?;
            let (tx, rx) = oneshot::channel();
            tokio::time::timeout(IO_TIMEOUT, async {
                terminal
                    .resize
                    .send(Resize { size, response: tx })
                    .await
                    .context("Terminal closed")?;
                rx.await.context("Terminal closed")?
            })
            .await
            .context("Terminal resize timed out")??;
            Ok(json!({"task_id":job.id,"rows":size.rows,"cols":size.cols}))
        }
        _ => bail!("Unknown terminal tool"),
    }
}

fn size(args: &Value) -> Result<PtySize> {
    let dimension = |key: &str| -> Result<u16> {
        let value = args[key].as_u64().context("Missing terminal dimension")?;
        ensure!(
            (1..=500).contains(&value),
            "Terminal dimensions must be 1..=500"
        );
        Ok(value as u16)
    };
    Ok(PtySize {
        rows: dimension("rows")?,
        cols: dimension("cols")?,
        pixel_width: 0,
        pixel_height: 0,
    })
}

struct Process {
    master: Option<Box<dyn MasterPty + Send>>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    group: ProcessGroup,
}
impl Process {
    fn kill(&mut self) {
        #[cfg(unix)]
        if let (Some(master), Some(pid)) = (&self.master, self.child.process_id())
            && let Some(foreground) = master.process_group_leader()
            // Job-control shells may move the foreground command into another group.
            && foreground > 0
            && unsafe { libc::getsid(foreground) } == pid as libc::pid_t
        {
            unsafe {
                libc::kill(-foreground, libc::SIGKILL);
            }
        }
        self.group.kill();
        let _ = self.child.kill();
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        self.kill();
    }
}
struct Spawned {
    process: Process,
    reader: Box<dyn Read + Send>,
    writer: Box<dyn Write + Send>,
}

fn spawn(cwd: &Path, command: &str, size: PtySize) -> Result<Spawned> {
    let pair = portable_pty::native_pty_system().openpty(size)?;
    #[cfg(windows)]
    let mut cmd = {
        // Encoding avoids cmd.exe's incompatible quoting rules for embedded quotes.
        let encoded: Vec<u8> = command.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let system_root = std::env::var_os("SYSTEMROOT")
            .or_else(|| std::env::var_os("WINDIR"))
            .context("Missing Windows system directory")?;
        let shell = std::path::PathBuf::from(system_root)
            .join("System32/WindowsPowerShell/v1.0/powershell.exe");
        let mut cmd = CommandBuilder::new(shell);
        cmd.args([
            "-NoLogo",
            "-NoProfile",
            "-EncodedCommand",
            &STANDARD.encode(encoded),
        ]);
        cmd
    };
    #[cfg(not(windows))]
    let mut cmd = {
        let mut cmd = CommandBuilder::new("/bin/sh");
        cmd.args(["-c", command]);
        cmd
    };
    cmd.cwd(cwd);
    cmd.env_clear();
    for key in EXECUTION_ENV {
        if let Some(value) = std::env::var_os(key) {
            cmd.env(key, value);
        }
    }
    cmd.env("TERM", "xterm-256color");
    let mut child = pair.slave.spawn_command(cmd)?;
    drop(pair.slave);
    let group = match ProcessGroup::for_pty(child.as_mut()) {
        Ok(group) => group,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
    };
    let process = Process {
        master: Some(pair.master),
        child,
        group,
    };
    let master = process.master.as_ref().expect("master");
    #[cfg(unix)]
    {
        let fd = master
            .as_raw_fd()
            .context("Native PTY has no file descriptor")?;
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        ensure!(
            flags >= 0 && unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } >= 0,
            "Cannot configure nonblocking terminal"
        );
    }
    let reader = master.try_clone_reader()?;
    let writer = master.take_writer()?;
    Ok(Spawned {
        process,
        reader,
        writer,
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn start(
    jobs: &Jobs,
    cwd: &Path,
    command: &str,
    timeout: u64,
    size: PtySize,
    parent: Option<String>,
    cancel: &CancellationToken,
    peer: &Peer,
    session: &str,
    limit: usize,
) -> Result<Value> {
    let job = jobs
        .reserve("terminal", parent, cancel.child_token())
        .await?;
    let directory = cwd.to_owned();
    let command = command.to_owned();
    let spawned = match tokio::task::spawn_blocking(move || spawn(&directory, &command, size)).await
    {
        Ok(result) => result,
        Err(error) => Err(anyhow::anyhow!("Terminal start interrupted: {error}")),
    };
    let spawned = match spawned {
        Ok(spawned) => spawned,
        Err(error) => {
            job.finish("failed", format!("{error:#}"), None, limit)
                .await;
            return Err(error);
        }
    };
    if job.cancel.is_cancelled() {
        drop(spawned);
        job.finish("cancelled", String::new(), None, limit).await;
        bail!("Cancelled");
    }
    let (input, writer_rx) = std::sync::mpsc::sync_channel(1);
    let (resize, resize_rx) = mpsc::channel(4);
    let (output_tx, output_rx) = mpsc::channel(4);
    let reader_stop = CancellationToken::new();
    let writer_cancel = job.cancel.clone();
    let writer_stop = reader_stop.clone();
    let reader_cancel = reader_stop.clone();
    let Spawned {
        process,
        reader,
        writer,
    } = spawned;
    // Persistent PTY I/O gets small dedicated stacks, never Tokio's shared blocking pool.
    let reader_thread = std::thread::Builder::new()
        .name("piko-pty-read".into())
        .stack_size(128 * 1024)
        .spawn(move || read_output(reader, output_tx, reader_cancel));
    if let Err(error) = reader_thread {
        drop(process);
        job.finish("failed", error.to_string(), None, limit).await;
        return Err(error.into());
    }
    let writer_thread = std::thread::Builder::new()
        .name("piko-pty-write".into())
        .stack_size(128 * 1024)
        .spawn(move || write_input(writer, writer_rx, writer_cancel, writer_stop));
    if let Err(error) = writer_thread {
        // A full output queue must not stall ConPTY teardown after thread creation fails.
        drop(output_rx);
        reader_stop.cancel();
        drop(process);
        job.finish("failed", error.to_string(), None, limit).await;
        return Err(error.into());
    }
    *job.terminal.lock().await = Some(Arc::new(Terminal { input, resize }));
    let monitor_job = job.clone();
    let monitor_peer = peer.clone();
    let monitor_session = session.to_owned();
    tokio::spawn(async move {
        monitor(
            process,
            output_rx,
            resize_rx,
            reader_stop,
            monitor_job,
            timeout,
            limit,
            monitor_peer,
            monitor_session,
        )
        .await;
    });
    peer.task_event(
        session,
        &job.id,
        json!({"status":"running","kind":"terminal","rows":size.rows,"cols":size.cols}),
    )
    .await?;
    Ok(job.snapshot().await)
}

fn read_output(
    mut reader: Box<dyn Read + Send>,
    sender: mpsc::Sender<Vec<u8>>,
    stop: CancellationToken,
) {
    let mut buffer = [0u8; 4096];
    let mut discard = false;
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                if !discard && sender.blocking_send(buffer[..count].to_vec()).is_err() {
                    discard = true;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if stop.is_cancelled() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            // Linux PTYs report EIO after their slave is closed.
            Err(_) => break,
        }
    }
}

async fn queue_input(sender: &std::sync::mpsc::SyncSender<Input>, mut input: Input) -> Result<()> {
    loop {
        match sender.try_send(input) {
            Ok(()) => return Ok(()),
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => bail!("Terminal input closed"),
            Err(std::sync::mpsc::TrySendError::Full(pending)) => input = pending,
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn write_input(
    mut writer: Box<dyn Write + Send>,
    receiver: std::sync::mpsc::Receiver<Input>,
    cancel: CancellationToken,
    stop: CancellationToken,
) {
    while !cancel.is_cancelled() && !stop.is_cancelled() {
        let input = match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(input) => input,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };
        // ConPTY interprets carriage return as Enter. Accept ordinary LF/CRLF
        // tool input consistently without expanding its byte budget.
        #[cfg(windows)]
        let input = Input {
            data: String::from_utf8_lossy(&input.data)
                .replace("\r\n", "\r")
                .replace('\n', "\r")
                .into_bytes(),
            response: input.response,
        };
        let deadline = Instant::now() + IO_TIMEOUT;
        let mut written = 0;
        let result: Result<()> = (|| {
            while written < input.data.len() {
                ensure!(
                    !cancel.is_cancelled() && !stop.is_cancelled() && Instant::now() < deadline,
                    "Terminal input cancelled or timed out"
                );
                match writer.write(&input.data[written..]) {
                    Ok(0) => bail!("Terminal input closed"),
                    Ok(count) => written += count,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5))
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            writer.flush()?;
            Ok(())
        })();
        let failed = result.is_err();
        let _ = input.response.send(result);
        if failed {
            cancel.cancel();
        }
        if cancel.is_cancelled() {
            break;
        }
    }
}

#[derive(Default)]
struct Decoder {
    pending: Vec<u8>,
}
impl Decoder {
    fn push(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        let mut offset = 0;
        let mut end = self.pending.len();
        loop {
            match std::str::from_utf8(&self.pending[offset..]) {
                Ok(_) => break,
                Err(error) => match error.error_len() {
                    Some(length) => offset += error.valid_up_to() + length,
                    None => {
                        end = offset + error.valid_up_to();
                        break;
                    }
                },
            }
        }
        let output = String::from_utf8_lossy(&self.pending[..end]).into_owned();
        self.pending.drain(..end);
        output
    }
    fn finish(self) -> String {
        String::from_utf8_lossy(&self.pending).into_owned()
    }
}

#[allow(clippy::too_many_arguments)]
async fn monitor(
    mut process: Process,
    mut output: mpsc::Receiver<Vec<u8>>,
    mut resize: mpsc::Receiver<Resize>,
    reader_stop: CancellationToken,
    job: Arc<Job>,
    timeout: u64,
    limit: usize,
    peer: Peer,
    session: String,
) {
    let deadline = tokio::time::sleep(Duration::from_secs(timeout));
    tokio::pin!(deadline);
    let mut poll = tokio::time::interval(Duration::from_millis(25));
    let mut decoder = Decoder::default();
    let mut output_open = true;
    let (status, code) = loop {
        tokio::select! {
            biased;
            _ = job.cancel.cancelled() => break ("cancelled", None),
            _ = &mut deadline => break ("timed_out", None),
            Some(request) = resize.recv() => { let result = process.master.as_ref().context("Terminal closed").and_then(|master|master.resize(request.size)); let _ = request.response.send(result); },
            chunk = output.recv(), if output_open => match chunk { Some(bytes) => job.append_output(&decoder.push(&bytes), limit).await, None => output_open = false },
            _ = poll.tick() => match process.child.try_wait() {
                Ok(Some(exit)) => break (if exit.success() { "completed" } else { "failed" }, i32::try_from(exit.exit_code()).ok()),
                Ok(None) => {}, Err(_) => break ("failed", None),
            },
        }
    };
    #[cfg(unix)]
    if let Some(pid) = process.child.process_id() {
        kill_session(pid).await;
    }
    process.kill();
    job.terminal.lock().await.take();
    reader_stop.cancel();
    let closing = tokio::task::spawn_blocking(move || {
        let _ = process.child.wait();
        drop(process);
    });
    // Keep draining while ConPTY closes; its ClosePseudoConsole can wait on its output pipe.
    let _ = tokio::time::timeout(Duration::from_secs(1), async {
        while let Some(bytes) = output.recv().await {
            job.append_output(&decoder.push(&bytes), limit).await;
        }
        let _ = closing.await;
    })
    .await;
    drop(output);
    job.append_output(&decoder.finish(), limit).await;
    let mut state = job.state.lock().await;
    state.status = status.into();
    state.exit_code = code;
    drop(state);
    job.done.notify_waiters();
    let _ = peer
        .task_event(&session, &job.id, job.snapshot().await)
        .await;
}

#[cfg(unix)]
async fn kill_session(session: u32) {
    use tokio::{io::AsyncReadExt, process::Command};
    // Interactive job-control shells create multiple groups. Target only processes
    // whose kernel session ID matches this PTY's freshly-created private session.
    // ps exposes IDs only: no command lines, credentials, or other user content.
    let sweep = async {
        let mut child = Command::new("/bin/ps")
            .args(["-e", "-o", "pid="])
            .env_clear()
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let mut bytes = Vec::new();
        child
            .stdout
            .take()
            .context("Missing process list")?
            .take(256 * 1024)
            .read_to_end(&mut bytes)
            .await?;
        let _ = child.wait().await?;
        for pid in String::from_utf8_lossy(&bytes)
            .split_whitespace()
            .rev()
            .filter_map(|pid| pid.parse::<i32>().ok())
        {
            if pid > 0 && unsafe { libc::getsid(pid) } == session as libc::pid_t {
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
            }
        }
        Ok::<_, anyhow::Error>(())
    };
    let _ = tokio::time::timeout(Duration::from_millis(500), sweep).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_output_survives_every_boundary_and_bounds_partial_bytes() {
        let text = "hello 世界 😀 fin";
        for split in 0..=text.len() {
            let mut decoder = Decoder::default();
            let first = decoder.push(&text.as_bytes()[..split]);
            assert!(decoder.pending.len() <= 3);
            let second = decoder.push(&text.as_bytes()[split..]);
            assert_eq!(format!("{first}{second}{}", decoder.finish()), text);
        }
        let mut decoder = Decoder::default();
        assert_eq!(decoder.push(&[0xff, b'a', 0xe4]), "�a");
        assert_eq!(decoder.push(&[0xb8, 0x96]), "世");
        assert!(decoder.finish().is_empty());
    }

    #[test]
    fn terminal_dimensions_are_strictly_bounded() {
        for value in [
            json!({"rows":0,"cols":80}),
            json!({"rows":24,"cols":501}),
            json!({"rows":-1,"cols":80}),
            json!({"rows":24.5,"cols":80}),
        ] {
            assert!(size(&value).is_err());
        }
        assert_eq!(size(&json!({"rows":500,"cols":1})).unwrap().rows, 500);
        assert_eq!(definitions().len(), 4);
    }

    async fn wait_finished(jobs: &Jobs, id: &str) -> Value {
        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                let result = jobs
                    .status(id, None, 1000, &CancellationToken::new())
                    .await
                    .unwrap();
                if result["status"] != "running" {
                    return result;
                }
            }
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    #[cfg(any(unix, windows))]
    async fn native_terminal_accepts_input_resizes_and_preserves_unicode() {
        let directory = tempfile::tempdir().unwrap();
        let jobs = Jobs::new(2, 4096);
        let (peer, mut receiver) = Peer::channel(32);
        let events = tokio::spawn(async move { while receiver.recv().await.is_some() {} });
        #[cfg(unix)]
        let command = "test -t 0 && test -t 1 || exit 9; printf 'ready>'; IFS= read -r line; printf 'reply:%s\\n' \"$line\"; stty size";
        #[cfg(windows)]
        let command = "if ([Console]::IsInputRedirected) {exit 9}; Write-Host 'ready>'; $line = Read-Host; Write-Host ('reply:' + $line)";
        let snapshot = start(
            &jobs,
            directory.path(),
            command,
            10,
            PtySize::default(),
            Some("owner".into()),
            &CancellationToken::new(),
            &peer,
            "pty-test",
            4096,
        )
        .await
        .unwrap();
        let id = snapshot["task_id"].as_str().unwrap();
        let job = jobs.get(id).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let snapshot = jobs
                    .status(id, Some("owner"), 1000, &CancellationToken::new())
                    .await
                    .unwrap();
                if snapshot["output"].as_str().unwrap().contains("ready>") {
                    break;
                }
                assert_eq!(snapshot["status"], "running", "{snapshot}");
            }
        })
        .await
        .unwrap();
        assert!(
            jobs.status(id, Some("different-owner"), 0, &CancellationToken::new())
                .await
                .is_err()
        );
        let terminal = job.terminal.lock().await.clone().unwrap();
        let (tx, rx) = oneshot::channel();
        terminal
            .resize
            .send(Resize {
                size: PtySize {
                    rows: 39,
                    cols: 105,
                    pixel_width: 0,
                    pixel_height: 0,
                },
                response: tx,
            })
            .await
            .unwrap();
        rx.await.unwrap().unwrap();
        let (tx, rx) = oneshot::channel();
        terminal
            .input
            .try_send(Input {
                data: "héllo\n".as_bytes().to_vec(),
                response: tx,
            })
            .unwrap();
        rx.await.unwrap().unwrap();
        drop(terminal);
        let finished = wait_finished(&jobs, id).await;
        assert_eq!(finished["status"], "completed", "{finished}");
        assert!(
            finished["output"].as_str().unwrap().contains("reply:héllo"),
            "{finished}"
        );
        #[cfg(unix)]
        assert!(
            finished["output"].as_str().unwrap().contains("39 105"),
            "{finished}"
        );
        assert!(job.terminal.lock().await.is_none());
        events.abort();
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn timeout_cleans_descendants_and_output_stays_bounded() {
        let directory = tempfile::tempdir().unwrap();
        let jobs = Jobs::new(2, 128);
        let (peer, mut receiver) = Peer::channel(32);
        let events = tokio::spawn(async move { while receiver.recv().await.is_some() {} });
        let cancel = CancellationToken::new();
        let started = start(
            &jobs,
            directory.path(),
            "sleep 30 & printf 'child:%s\\n' \"$!\"; wait",
            1,
            PtySize::default(),
            None,
            &cancel,
            &peer,
            "pty-timeout",
            128,
        )
        .await
        .unwrap();
        let id = started["task_id"].as_str().unwrap();
        let finished = wait_finished(&jobs, id).await;
        assert_eq!(finished["status"], "timed_out", "{finished}");
        let output = finished["output"].as_str().unwrap();
        let pid: i32 = output
            .trim()
            .strip_prefix("child:")
            .unwrap()
            .parse()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while unsafe { libc::kill(pid, 0) } == 0 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        let started = start(
            &jobs,
            directory.path(),
            "printf '%06000d' 0",
            5,
            PtySize::default(),
            None,
            &cancel,
            &peer,
            "pty-output",
            128,
        )
        .await
        .unwrap();
        let finished = wait_finished(&jobs, started["task_id"].as_str().unwrap()).await;
        assert_eq!(finished["status"], "completed", "{finished}");
        assert_eq!(finished["truncated"], true);
        assert!(finished["output"].as_str().unwrap().len() <= 128);
        events.abort();
    }

    #[tokio::test]
    #[cfg(any(unix, windows))]
    async fn cancellation_releases_terminal_and_job_capacity() {
        let directory = tempfile::tempdir().unwrap();
        let jobs = Jobs::new(1, 128);
        let (peer, mut receiver) = Peer::channel(32);
        let events = tokio::spawn(async move { while receiver.recv().await.is_some() {} });
        #[cfg(unix)]
        let command = "sleep 30";
        #[cfg(windows)]
        let command = "Start-Sleep -Seconds 30";
        let started = start(
            &jobs,
            directory.path(),
            command,
            30,
            PtySize::default(),
            Some("owner".into()),
            &CancellationToken::new(),
            &peer,
            "pty-cancel",
            128,
        )
        .await
        .unwrap();
        let job = jobs
            .get(started["task_id"].as_str().unwrap())
            .await
            .unwrap();
        job.cancel.cancel();
        let finished = wait_finished(&jobs, &job.id).await;
        assert_eq!(finished["status"], "cancelled");
        assert!(job.terminal.lock().await.is_none());
        assert!(
            jobs.reserve("test", None, CancellationToken::new())
                .await
                .is_ok()
        );
        events.abort();
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

    #[test]
    fn writer_stops_even_when_an_approval_waiter_retains_its_sender() {
        struct Writer(std::sync::mpsc::Sender<()>);
        impl Write for Writer {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl Drop for Writer {
            fn drop(&mut self) {
                let _ = self.0.send(());
            }
        }
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let (closed, finished) = std::sync::mpsc::channel();
        let stop = CancellationToken::new();
        let worker_stop = stop.clone();
        let worker = std::thread::spawn(move || {
            write_input(
                Box::new(Writer(closed)),
                receiver,
                CancellationToken::new(),
                worker_stop,
            )
        });
        stop.cancel();
        finished.recv_timeout(Duration::from_secs(1)).unwrap();
        // Still retain sender here: terminal shutdown cannot depend on its last clone dropping.
        assert!(
            sender
                .try_send(Input {
                    data: vec![b'x'],
                    response: oneshot::channel().0
                })
                .is_err()
        );
        worker.join().unwrap();
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn canonical_terminal_eof_reaches_the_program() {
        let directory = tempfile::tempdir().unwrap();
        let jobs = Jobs::new(1, 1024);
        let (peer, mut updates) = Peer::channel(32);
        let events = tokio::spawn(async move { while updates.recv().await.is_some() {} });
        let started = start(
            &jobs,
            directory.path(),
            "printf 'ready>'; IFS= read -r line || printf 'eof-received\\n'",
            5,
            PtySize::default(),
            None,
            &CancellationToken::new(),
            &peer,
            "pty-eof",
            1024,
        )
        .await
        .unwrap();
        let id = started["task_id"].as_str().unwrap();
        let job = jobs.get(id).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let snapshot = jobs
                    .status(id, None, 100, &CancellationToken::new())
                    .await
                    .unwrap();
                if snapshot["output"].as_str().unwrap().contains("ready>") {
                    break;
                }
            }
        })
        .await
        .unwrap();
        let terminal = job.terminal.lock().await.clone().unwrap();
        let (tx, rx) = oneshot::channel();
        terminal
            .input
            .try_send(Input {
                data: vec![4],
                response: tx,
            })
            .unwrap();
        rx.await.unwrap().unwrap();
        let finished = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let snapshot = jobs
                    .status(id, None, 100, &CancellationToken::new())
                    .await
                    .unwrap();
                if snapshot["status"] != "running" {
                    break snapshot;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(finished["status"], "completed", "{finished}");
        assert!(
            finished["output"]
                .as_str()
                .unwrap()
                .contains("eof-received"),
            "{finished}"
        );
        events.abort();
    }
}
