use crate::{protocol::Peer, workspace::truncate};
use anyhow::{Result, bail, ensure};
use serde_json::{Value, json};
use std::{collections::BTreeMap, process::Stdio, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    sync::{Mutex, Notify},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Clone)]
pub struct Jobs {
    entries: Arc<Mutex<BTreeMap<String, Arc<Job>>>>,
    limit: usize,
    output_limit: usize,
}

pub struct Job {
    pub id: String,
    pub kind: String,
    pub parent: Option<String>,
    pub cancel: CancellationToken,
    pub state: Mutex<JobState>,
    pub done: Notify,
}

pub struct JobState {
    pub status: String,
    pub output: String,
    pub exit_code: Option<i32>,
    pub truncated: bool,
}

impl Jobs {
    pub fn new(limit: usize, output_limit: usize) -> Self {
        Self {
            entries: Arc::new(Mutex::new(BTreeMap::new())),
            limit,
            output_limit,
        }
    }

    pub async fn reserve(
        &self,
        kind: &str,
        parent: Option<String>,
        cancel: CancellationToken,
    ) -> Result<Arc<Job>> {
        let mut entries = self.entries.lock().await;
        if entries.len() >= self.limit {
            let mut completed = None;
            for (id, job) in entries.iter() {
                if job.state.lock().await.status != "running" {
                    completed = Some(id.clone());
                    break;
                }
            }
            if let Some(id) = completed {
                entries.remove(&id);
            } else {
                bail!("Background task limit reached; wait for or cancel a task");
            }
        }
        anyhow::ensure!(!cancel.is_cancelled(), "Task cancelled before starting");
        let job = Arc::new(Job {
            id: Uuid::new_v4().to_string(),
            kind: kind.into(),
            parent,
            cancel,
            state: Mutex::new(JobState {
                status: "running".into(),
                output: String::new(),
                exit_code: None,
                truncated: false,
            }),
            done: Notify::new(),
        });
        entries.insert(job.id.clone(), job.clone());
        Ok(job)
    }

    pub async fn get(&self, id: &str) -> Result<Arc<Job>> {
        self.entries
            .lock()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Unknown or evicted task"))
    }

    pub async fn status(
        &self,
        id: &str,
        parent: Option<&str>,
        wait_ms: u64,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        let job = self.get(id).await?;
        ensure!(
            parent.is_none() || job.parent.as_deref() == parent,
            "Task belongs to another agent"
        );
        let notified = job.done.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if job.state.lock().await.status == "running" && wait_ms > 0 {
            tokio::select! {
                _ = cancel.cancelled() => bail!("Cancelled"),
                _ = tokio::time::sleep(Duration::from_millis(wait_ms.min(30000))) => {},
                _ = notified => {},
            }
        }
        Ok(job.snapshot().await)
    }

    pub async fn list(&self) -> Vec<Value> {
        let jobs: Vec<_> = self.entries.lock().await.values().cloned().collect();
        let mut result = Vec::new();
        for job in jobs {
            result.push(job.snapshot().await);
        }
        result
    }

    pub async fn cancel_all(&self) {
        for job in self.entries.lock().await.values() {
            job.cancel.cancel();
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn shell(
        &self,
        cwd: &std::path::Path,
        command: &str,
        timeout: u64,
        background: bool,
        parent: Option<String>,
        cancel: &CancellationToken,
        peer: &Peer,
        session: &str,
    ) -> Result<Value> {
        let job = self.reserve("shell", parent, cancel.child_token()).await?;
        #[cfg(windows)]
        let mut process = {
            use std::os::windows::process::CommandExt;
            let mut command_builder = Command::new("cmd.exe");
            // cmd.exe uses shell quoting rather than C argv quoting. /S strips
            // the wrapper quotes while preserving quoted paths in the command.
            command_builder
                .as_std_mut()
                .raw_arg(format!("/D /S /C \"{command}\""));
            command_builder
        };
        #[cfg(not(windows))]
        let mut process = {
            let mut command_builder = Command::new("/bin/sh");
            command_builder.args(["-c", command]);
            command_builder
        };
        process
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .env_clear();
        inherit_process_environment(&mut process);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            process.as_std_mut().process_group(0);
        }
        let mut child = match process.spawn() {
            Ok(child) => child,
            Err(error) => {
                job.finish("failed", error.to_string(), None, self.output_limit)
                    .await;
                return Err(error.into());
            }
        };
        let mut group = match ProcessGroup::new(&mut child) {
            Ok(group) => group,
            Err(error) => {
                let _ = child.kill().await;
                job.finish("failed", error.to_string(), None, self.output_limit)
                    .await;
                return Err(error);
            }
        };
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let job_clone = job.clone();
        let monitor_peer = peer.clone();
        let monitor_session = session.to_owned();
        let limit = self.output_limit;
        tokio::spawn(async move {
            let mut out = tokio::spawn(drain(stdout, job_clone.clone(), limit));
            let mut err = tokio::spawn(drain(stderr, job_clone.clone(), limit));
            let (status, code) = tokio::select! {
                biased;
                _ = job_clone.cancel.cancelled() => ("cancelled", None),
                _ = tokio::time::sleep(Duration::from_secs(timeout)) => ("timed_out", None),
                result = child.wait() => match result {
                    Ok(exit) => (if exit.success() { "completed" } else { "failed" }, exit.code()),
                    Err(_) => ("failed", None),
                }
            };
            group.kill();
            let _ = child.kill().await;
            let _ = child.wait().await;
            // Descendants may inherit pipes; never let them keep a monitor alive forever.
            if tokio::time::timeout(Duration::from_secs(1), async {
                let _ = (&mut out).await;
                let _ = (&mut err).await;
            })
            .await
            .is_err()
            {
                out.abort();
                err.abort();
            }
            {
                let mut state = job_clone.state.lock().await;
                state.status = status.into();
                state.exit_code = code;
            }
            job_clone.done.notify_waiters();
            let _ = monitor_peer
                .task_event(&monitor_session, &job_clone.id, job_clone.snapshot().await)
                .await;
        });
        peer.task_event(
            session,
            &job.id,
            json!({"status":"running", "kind":"shell"}),
        )
        .await?;
        if background {
            Ok(json!({"task_id":job.id,"status":"running"}))
        } else {
            loop {
                let result = self.status(&job.id, None, 30000, cancel).await?;
                if result["status"] != "running" {
                    break Ok(result);
                }
            }
        }
    }
}

impl Job {
    pub async fn snapshot(&self) -> Value {
        let state = self.state.lock().await;
        json!({"task_id":self.id,"kind":self.kind,"parent_id":self.parent,"status":state.status,"output":state.output,"exit_code":state.exit_code,"truncated":state.truncated})
    }

    pub async fn finish(&self, status: &str, output: String, code: Option<i32>, limit: usize) {
        let mut state = self.state.lock().await;
        state.status = status.into();
        state.truncated = output.len() > limit;
        state.output = truncate(&output, limit);
        state.exit_code = code;
        drop(state);
        self.done.notify_waiters();
    }
}

async fn drain(mut reader: impl AsyncRead + Unpin, job: Arc<Job>, limit: usize) {
    let mut buffer = [0u8; 4096];
    while let Ok(count) = reader.read(&mut buffer).await {
        if count == 0 {
            break;
        }
        let mut state = job.state.lock().await;
        state
            .output
            .push_str(&String::from_utf8_lossy(&buffer[..count]));
        if state.output.len() > limit {
            let mut start = state.output.len() - limit;
            while !state.output.is_char_boundary(start) {
                start += 1;
            }
            state.output.drain(..start);
            state.truncated = true;
        }
    }
}

// Forward only execution essentials, never the host's API keys or auth tokens.
pub(crate) fn inherit_process_environment(command: &mut Command) {
    for key in [
        "PATH",
        "HOME",
        "USER",
        "LANG",
        "LC_ALL",
        "TMPDIR",
        "XDG_RUNTIME_DIR",
        "SYSTEMROOT",
        "WINDIR",
        "COMSPEC",
        "PATHEXT",
        "TEMP",
        "TMP",
        "USERPROFILE",
        "HOMEDRIVE",
        "HOMEPATH",
        "LOCALAPPDATA",
        "APPDATA",
    ] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
}

pub(crate) struct ProcessGroup {
    #[cfg(unix)]
    pid: Option<u32>,
    #[cfg(windows)]
    job: std::os::windows::io::OwnedHandle,
}

impl ProcessGroup {
    pub(crate) fn new(child: &mut tokio::process::Child) -> Result<Self> {
        #[cfg(windows)]
        {
            use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
            use windows_sys::Win32::System::JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
                SetInformationJobObject,
            };
            // The owned handle closes even when setting limits or assignment fails.
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            ensure!(
                !handle.is_null(),
                "Cannot create process job: {}",
                std::io::Error::last_os_error()
            );
            let job = unsafe { OwnedHandle::from_raw_handle(handle) };
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let configured = unsafe {
                SetInformationJobObject(
                    job.as_raw_handle(),
                    JobObjectExtendedLimitInformation,
                    (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                    std::mem::size_of_val(&limits) as u32,
                )
            };
            ensure!(
                configured != 0,
                "Cannot configure process job: {}",
                std::io::Error::last_os_error()
            );
            let process = child.raw_handle().context("Missing child process handle")?;
            let assigned = unsafe { AssignProcessToJobObject(job.as_raw_handle(), process) };
            if assigned == 0 {
                let error = std::io::Error::last_os_error();
                // Very short commands can exit before assignment. Preserve their
                // cached exit status; fail closed if a live child is uncontained.
                ensure!(
                    child.try_wait()?.is_some(),
                    "Cannot contain process tree: {error}"
                );
            }
            Ok(Self { job })
        }
        #[cfg(unix)]
        {
            Ok(Self { pid: child.id() })
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = child;
            Ok(Self {})
        }
    }

    pub(crate) fn kill(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.pid.take() {
            // Child is started in its own process group; kill descendants, not the host.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            unsafe {
                windows_sys::Win32::System::JobObjects::TerminateJobObject(
                    self.job.as_raw_handle(),
                    1,
                );
            }
        }
    }
}
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.kill();
    }
}

#[cfg(windows)]
use anyhow::Context;

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_shell_supports_home_environment_and_quoted_paths() {
        let directory = tempfile::tempdir().unwrap();
        let jobs = Jobs::new(4, 1024);
        let (peer, mut updates) = Peer::channel(32);
        let drain = tokio::spawn(async move { while updates.recv().await.is_some() {} });
        for command in [
            "if not defined USERPROFILE exit /b 7",
            "echo ok>\"file with spaces.txt\" & type \"file with spaces.txt\"",
        ] {
            let output = jobs
                .shell(
                    directory.path(),
                    command,
                    5,
                    false,
                    None,
                    &CancellationToken::new(),
                    &peer,
                    "windows-test",
                )
                .await
                .unwrap();
            assert_eq!(output["status"], "completed", "{output}");
        }
        assert_eq!(
            std::fs::read_to_string(directory.path().join("file with spaces.txt"))
                .unwrap()
                .trim(),
            "ok"
        );
        drain.abort();
    }

    #[tokio::test]
    #[cfg(any(unix, windows))]
    async fn process_container_terminates_its_child() {
        #[cfg(windows)]
        let mut command = {
            let mut command = Command::new("cmd.exe");
            command.args(["/D", "/C", "ping -n 30 127.0.0.1 >nul"]);
            command
        };
        #[cfg(unix)]
        let mut command = {
            use std::os::unix::process::CommandExt;
            let mut command = Command::new("/bin/sh");
            command.args(["-c", "sleep 30"]);
            command.as_std_mut().process_group(0);
            command
        };
        command
            .kill_on_drop(true)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = command.spawn().unwrap();
        let mut group = ProcessGroup::new(&mut child).unwrap();
        group.kill();
        let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(!status.success());
    }
}
