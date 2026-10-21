use crate::{
    agent::AgentContext,
    config::Config,
    jobs::Jobs,
    mcp::{Pool as McpPool, Servers as McpServers},
    provider::Provider,
    session::{Session, Store, Turn},
    tools::string,
    workspace::{Workspace, read_bounded},
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter},
    sync::{Mutex, Semaphore, mpsc, oneshot},
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub const MAX_FRAME: usize = 1024 * 1024;

#[derive(Clone)]
pub struct Peer {
    tx: mpsc::Sender<Value>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>,
}

impl Peer {
    pub fn channel(capacity: usize) -> (Self, mpsc::Receiver<Value>) {
        let (tx, rx) = mpsc::channel(capacity);
        (
            Self {
                tx,
                pending: Arc::new(Mutex::new(HashMap::new())),
            },
            rx,
        )
    }

    pub async fn send(&self, value: Value) -> Result<()> {
        self.tx.send(value).await.context("ACP client disconnected")
    }
    pub async fn update(&self, session: &str, update: Value) -> Result<()> {
        self.send(json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":session,"update":update}})).await
    }
    pub async fn task_event(&self, session: &str, id: &str, event: Value) -> Result<()> {
        self.send(json!({"jsonrpc":"2.0","method":"_harness/task","params":{"sessionId":session,"taskId":id,"event":event}})).await
    }

    pub async fn permission(
        &self,
        session: &str,
        call: &str,
        title: &str,
        kind: &str,
        input: Value,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let id = format!("permission-{}", Uuid::new_v4());
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id.clone(), tx);
        let result: Result<Value> = async {
            self.send(json!({"jsonrpc":"2.0","id":id,"method":"session/request_permission","params":{
                "sessionId":session,"toolCall":{"toolCallId":call,"title":title,"kind":kind,"status":"pending","rawInput":input},
                "options":[{"optionId":"allow-once","name":"Allow once","kind":"allow_once"},{"optionId":"reject-once","name":"Reject","kind":"reject_once"}]
            }})).await?;
            tokio::select! {
                biased;
                _ = cancel.cancelled() => bail!("Permission request cancelled"),
                result = tokio::time::timeout(Duration::from_secs(300),rx) => result.context("Permission request timed out")?.context("Permission response lost"),
            }
        }.await;
        self.pending.lock().await.remove(&id);
        let result = result?;
        ensure!(
            result["result"]["outcome"]["outcome"] == "selected"
                && result["result"]["outcome"]["optionId"] == "allow-once",
            "Permission denied; do not retry this action through another tool"
        );
        Ok(())
    }

    pub async fn resolve(&self, value: Value) {
        if let Some(id) = value["id"].as_str()
            && let Some(sender) = self.pending.lock().await.remove(id)
        {
            let _ = sender.send(value);
        }
    }
}

struct Slot {
    session: Mutex<Session>,
    workspace: Workspace,
    busy: Arc<Semaphore>,
    cancel: Mutex<CancellationToken>,
    jobs: Jobs,
    mcp: Arc<McpServers>,
}

pub struct Server {
    config: Arc<Config>,
    peer: Peer,
    provider: Provider,
    store: Store,
    sessions: Mutex<HashMap<Uuid, Arc<Slot>>>,
    initialized: AtomicBool,
    agents: Arc<Semaphore>,
    searches: Arc<Semaphore>,
    shutdown: CancellationToken,
    mcp_pool: Arc<McpPool>,
    desktop: Arc<crate::computer::Desktop>,
}

impl Server {
    pub async fn new(mut config: Config, peer: Peer) -> Result<Self> {
        config.validate()?;
        tokio::fs::create_dir_all(&config.state_dir).await?;
        config.state_dir = tokio::fs::canonicalize(&config.state_dir).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&config.state_dir, std::fs::Permissions::from_mode(0o700))
                .await?;
        }
        let store = Store::new(config.state_dir.join("sessions")).await?;
        let provider = Provider::new(&config)?;
        let mcp_pool = Arc::new(McpPool::new(config.max_mcp_connections));
        Ok(Self {
            agents: Arc::new(Semaphore::new(config.max_subagents)),
            searches: Arc::new(Semaphore::new(config.max_searches)),
            config: Arc::new(config),
            peer,
            provider,
            store,
            sessions: Mutex::new(HashMap::new()),
            initialized: AtomicBool::new(false),
            shutdown: CancellationToken::new(),
            mcp_pool,
            desktop: Arc::new(crate::computer::Desktop::default()),
        })
    }

    pub async fn handle(&self, method: &str, params: Value) -> Result<Value> {
        if method == "initialize" {
            ensure!(
                params["protocolVersion"].is_u64(),
                "Missing protocolVersion"
            );
            self.initialized.store(true, Ordering::Release);
            return Ok(
                json!({"protocolVersion":1,"agentInfo":{"name":"codex-harness","version":env!("CARGO_PKG_VERSION"),"title":"Codex Harness"},
                "agentCapabilities":{"loadSession":true,"promptCapabilities":{},"mcpCapabilities":{"http":true,"sse":false},"_meta":{"harness":{"tasks":true,"boundedContext":true,"computerUse":self.config.computer_use}}},
                "authMethods":[{"id":"chatgpt","name":"ChatGPT subscription","description":"Run codex-harness login before starting the agent"},{"id":"api_key","name":"Provider API key","description":"Set the configured api_key_env environment variable"}]}),
            );
        }
        ensure!(
            self.initialized.load(Ordering::Acquire),
            "Call initialize before session methods"
        );
        match method {
            "_harness/stats" => Ok(self.provider.metrics.snapshot()),
            "authenticate" => {
                let method = string(&params, "methodId")?;
                ensure!(
                    ["chatgpt", "api_key"].contains(&method),
                    "Unknown authentication method"
                );
                self.provider.authenticate(method).await?;
                Ok(json!({}))
            }
            "session/new" | "session/load" => {
                let mut workspace = Workspace::new(Path::new(string(&params, "cwd")?)).await?;
                workspace.protect(self.config.state_dir.clone());
                let mcp = Arc::new(McpServers::parse(
                    &params["mcpServers"],
                    &workspace.root,
                    self.mcp_pool.clone(),
                )?);
                let session = if method == "session/load" {
                    self.store
                        .load(Uuid::parse_str(string(&params, "sessionId")?)?, &workspace)
                        .await?
                } else {
                    Session::new(&workspace, &self.config)
                };
                let id = session.id;
                let mode = session.mode.clone();
                let model = session.model.clone();
                let slot = Arc::new(Slot {
                    session: Mutex::new(session),
                    workspace,
                    busy: Arc::new(Semaphore::new(0)),
                    cancel: Mutex::new(self.shutdown.child_token()),
                    jobs: Jobs::new(self.config.max_jobs, self.config.tool_output_bytes),
                    mcp,
                });
                let mut sessions = self.sessions.lock().await;
                ensure!(!sessions.contains_key(&id), "Session already loaded");
                ensure!(
                    sessions.len() < self.config.max_sessions,
                    "Loaded session limit reached; unload a session first"
                );
                sessions.insert(id, slot.clone());
                drop(sessions);
                let result: Result<()> = async {
                    let session = slot.session.lock().await;
                    self.store.save(&session).await?;
                    if method == "session/load" {
                        self.replay(&session).await?;
                    }
                    Ok(())
                }
                .await;
                if let Err(error) = result {
                    self.sessions.lock().await.remove(&id);
                    return Err(error);
                }
                slot.busy.add_permits(1);
                Ok(
                    json!({"sessionId":id,"modes":modes(&mode),"models":{"currentModelId":model,"availableModels":[{"modelId":model,"name":model}]}}),
                )
            }
            "session/prompt" => self.prompt(params).await,
            "session/cancel" => {
                let slot = self.slot(&params).await?;
                slot.cancel.lock().await.cancel();
                Ok(json!({}))
            }
            "session/set_model" => {
                let slot = self.slot(&params).await?;
                let _permit = slot
                    .busy
                    .clone()
                    .try_acquire_owned()
                    .context("Session is busy")?;
                let model = string(&params, "modelId")?;
                // Each session advertises its configured model only. ACP clients
                // may reselect that model before every prompt, including reloads.
                ensure!(
                    model == slot.session.lock().await.model,
                    "Model is not available in this session; use its advertised model or change the trusted configuration"
                );
                Ok(json!({}))
            }
            "session/set_mode" => {
                let slot = self.slot(&params).await?;
                let _permit = slot
                    .busy
                    .clone()
                    .try_acquire_owned()
                    .context("Session is busy")?;
                let mode = string(&params, "modeId")?;
                ensure!(["code", "plan"].contains(&mode), "Unknown mode");
                ensure!(
                    !slot
                        .jobs
                        .list()
                        .await
                        .iter()
                        .any(|j| j["status"] == "running"),
                    "Cancel running background tasks before changing mode"
                );
                let mut session = slot.session.lock().await;
                session.mode = mode.into();
                self.store.save(&session).await?;
                self.peer
                    .update(
                        &session.id.to_string(),
                        json!({"sessionUpdate":"current_mode_update","currentModeId":mode}),
                    )
                    .await?;
                Ok(json!({}))
            }
            "_harness/tasks" => {
                let slot = self.slot(&params).await?;
                Ok(json!({"tasks":slot.jobs.list().await}))
            }
            "_harness/task/cancel" => {
                let slot = self.slot(&params).await?;
                slot.jobs
                    .get(string(&params, "taskId")?)
                    .await?
                    .cancel
                    .cancel();
                Ok(json!({}))
            }
            "_harness/session/unload" => {
                let slot = self.slot(&params).await?;
                let _permit = slot
                    .busy
                    .clone()
                    .try_acquire_owned()
                    .context("Session is busy")?;
                slot.cancel.lock().await.cancel();
                slot.jobs.cancel_all().await;
                let id = Uuid::parse_str(string(&params, "sessionId")?)?;
                self.sessions.lock().await.remove(&id);
                Ok(json!({}))
            }
            _ => bail!("Method not found"),
        }
    }

    async fn slot(&self, params: &Value) -> Result<Arc<Slot>> {
        let id = Uuid::parse_str(string(params, "sessionId")?)?;
        self.sessions
            .lock()
            .await
            .get(&id)
            .cloned()
            .context("Unknown session; create or load it first")
    }

    async fn prompt(&self, params: Value) -> Result<Value> {
        let slot = self.slot(&params).await?;
        let _permit = slot
            .busy
            .clone()
            .try_acquire_owned()
            .context("Session is busy; cancel or wait for its active turn")?;
        let cancel = self.shutdown.child_token();
        *slot.cancel.lock().await = cancel.clone();
        let text = self.prompt_text(&params, &slot.workspace).await?;
        ensure!(!text.trim().is_empty(), "Prompt is empty");
        ensure!(
            text.len() <= self.config.context_bytes / 2,
            "Prompt exceeds context budget"
        );
        let mut session = slot.session.lock().await;
        let ctx = AgentContext {
            config: self.config.clone(),
            provider: self.provider.clone(),
            peer: self.peer.clone(),
            workspace: slot.workspace.clone(),
            jobs: slot.jobs.clone(),
            mcp: slot.mcp.clone(),
            desktop: self.desktop.clone(),
            agents: self.agents.clone(),
            searches: self.searches.clone(),
            session_id: session.id.to_string(),
            agent_id: None,
            model: session.model.clone(),
            depth: 0,
            read_only: session.mode == "plan",
            cancel: cancel.clone(),
        };
        let instructions = crate::agent::instructions(&ctx).await?;
        let toolset = match crate::agent::prepare(&ctx).await {
            Ok(toolset) => toolset,
            Err(_) if cancel.is_cancelled() => return Ok(json!({"stopReason":"cancelled"})),
            Err(error) => return Err(error),
        };
        let user = json!({"role":"user","content":text});
        let reserved = crate::session::json_bytes(&user)?
            + instructions.len()
            + crate::session::json_bytes(&toolset.definitions)?
            + 512;
        ensure!(
            reserved <= self.config.context_bytes,
            "Prompt and project instructions exceed context budget"
        );
        let mut items = session.context(self.config.context_bytes - reserved);
        if session.dropped_turns > 0 {
            items.insert(0,json!({"role":"developer","content":format!("{} older completed turns were evicted to bound memory. Do not assume their contents; re-read the current workspace as needed.",session.dropped_turns)}));
        }
        let start = items.len();
        items.push(user);
        let result =
            crate::agent::run_with_instructions(&ctx, &mut items, instructions, toolset).await;
        // Save complete exchanges only; interrupted responses never enter model history.
        let mut turn = items.split_off(start);
        repair_calls(&mut turn);
        session.turns.push_back(Turn { items: turn });
        session.trim(self.config.context_bytes);
        let saved = self.store.save(&session).await;
        *slot.cancel.lock().await = self.shutdown.child_token();
        saved?;
        if cancel.is_cancelled() {
            return Ok(json!({"stopReason":"cancelled"}));
        }
        let result = result?;
        Ok(json!({"stopReason":result.stop_reason}))
    }

    async fn prompt_text(&self, params: &Value, workspace: &Workspace) -> Result<String> {
        let blocks = params["prompt"]
            .as_array()
            .context("Missing prompt array")?;
        let mut text = String::new();
        for block in blocks {
            match block["type"].as_str() {
                Some("text") => text.push_str(string(block, "text")?),
                Some("resource_link") => {
                    let uri = string(block, "uri")?;
                    if uri.starts_with("file:") {
                        let path = reqwest::Url::parse(uri)?
                            .to_file_path()
                            .map_err(|_| anyhow::anyhow!("Invalid file resource URI"))?;
                        let path = workspace
                            .resolve(path.to_str().context("Invalid resource path")?, false)
                            .await?;
                        text.push_str(&format!(
                            "Resource {}:\n{}",
                            path.display(),
                            read_bounded(&path, self.config.tool_output_bytes).await?
                        ));
                    } else {
                        text.push_str(&format!("Resource link: {uri}"));
                    }
                }
                _ => bail!("Only text and resource_link prompts are supported"),
            }
            text.push('\n');
            ensure!(
                text.len() <= self.config.context_bytes / 2,
                "Prompt exceeds context budget"
            );
        }
        Ok(text)
    }

    async fn replay(&self, session: &Session) -> Result<()> {
        for turn in &session.turns {
            for item in &turn.items {
                if item["type"] == "function_call" {
                    let input = item["arguments"]
                        .as_str()
                        .and_then(|args| serde_json::from_str::<Value>(args).ok());
                    self.peer.update(&session.id.to_string(),json!({"sessionUpdate":"tool_call","toolCallId":item["call_id"],"title":item["name"],"kind":"other","status":"in_progress","rawInput":input})).await?;
                } else if item["type"] == "function_call_output" {
                    let text = crate::multimodal::text(&item["output"]);
                    let output = serde_json::from_str::<Value>(&text).ok();
                    let failed = text.starts_with("Tool reported an error.")
                        || output
                            .as_ref()
                            .is_some_and(|output| output.get("error").is_some());
                    self.peer.update(&session.id.to_string(),json!({"sessionUpdate":"tool_call_update","toolCallId":item["call_id"],"status":if failed {"failed"} else {"completed"},"content":crate::multimodal::acp_content(&item["output"])})).await?;
                }
                if let Some(role) = item["role"].as_str() {
                    let text = if let Some(text) = item["content"].as_str() {
                        text.to_owned()
                    } else {
                        item["content"]
                            .as_array()
                            .map(|c| {
                                c.iter()
                                    .filter_map(|c| c["text"].as_str())
                                    .collect::<Vec<_>>()
                                    .join("")
                            })
                            .unwrap_or_default()
                    };
                    if !text.is_empty() && ["user", "assistant"].contains(&role) {
                        self.peer.update(&session.id.to_string(),json!({"sessionUpdate":if role == "user" {"user_message_chunk"} else {"agent_message_chunk"},"content":{"type":"text","text":text}})).await?;
                    }
                }
            }
        }
        Ok(())
    }

    pub async fn shutdown(&self) {
        self.shutdown.cancel();
        for slot in self.sessions.lock().await.values() {
            slot.jobs.cancel_all().await;
        }
    }
}

pub fn repair_calls(items: &mut Vec<Value>) {
    let calls: Vec<_> = items
        .iter()
        .filter(|v| v["type"] == "function_call")
        .filter_map(|v| v["call_id"].as_str().map(str::to_owned))
        .collect();
    for id in calls {
        if !items
            .iter()
            .any(|v| v["type"] == "function_call_output" && v["call_id"] == id)
        {
            items.push(json!({"type":"function_call_output","call_id":id,"output":json!({"error":"Tool was interrupted and not executed to completion. Inspect current state before retrying."}).to_string()}));
        }
    }
}

fn modes(current: &str) -> Value {
    json!({"currentModeId":current,"availableModes":[{"id":"code","name":"Code","description":"Workspace reads; edits and shell require approval"},{"id":"plan","name":"Plan","description":"Read-only exploration; no writes or shell, including subagents"}]})
}

pub async fn read_frame(reader: &mut (impl AsyncBufRead + Unpin)) -> Result<Option<Vec<u8>>> {
    let mut frame = Vec::new();
    loop {
        let buffer = reader.fill_buf().await?;
        if buffer.is_empty() {
            return if frame.is_empty() {
                Ok(None)
            } else {
                Ok(Some(frame))
            };
        }
        let count = buffer
            .iter()
            .position(|b| *b == b'\n')
            .map_or(buffer.len(), |i| i + 1);
        ensure!(
            frame.len() + count <= MAX_FRAME,
            "ACP message exceeds 1MiB limit"
        );
        let complete = buffer[count - 1] == b'\n';
        frame.extend_from_slice(&buffer[..count]);
        reader.consume(count);
        if complete {
            return Ok(Some(frame));
        }
    }
}

pub async fn serve(config: Config) -> Result<()> {
    let (peer, mut outbound) = Peer::channel(64);
    let server = Arc::new(Server::new(config, peer.clone()).await?);
    let writer = tokio::spawn(async move {
        let mut stdout = BufWriter::new(tokio::io::stdout());
        while let Some(value) = outbound.recv().await {
            stdout.write_all(&serde_json::to_vec(&value)?).await?;
            stdout.write_all(b"\n").await?;
            stdout.flush().await?;
        }
        Ok::<_, anyhow::Error>(())
    });
    let mut reader = BufReader::new(tokio::io::stdin());
    let mut handlers = JoinSet::new();
    let signal = shutdown_signal();
    tokio::pin!(signal);
    let result: Result<()> = async {
        loop {
            let frame = tokio::select! {
                biased;
                _ = &mut signal => break,
                _ = peer.tx.closed() => break,
                frame = read_frame(&mut reader) => frame?,
            };
            let Some(frame) = frame else {
                break;
            };
            if frame.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let value: Value = match serde_json::from_slice(&frame) {
                Ok(value) => value,
                Err(_) => {
                    peer.send(error(Value::Null, -32700, "Parse error")).await?;
                    continue;
                }
            };
            if value["jsonrpc"] != "2.0" || !value.is_object() {
                peer.send(error(Value::Null, -32600, "Invalid JSON-RPC request"))
                    .await?;
                continue;
            }
            if value.get("method").is_none() {
                peer.resolve(value).await;
                continue;
            }
            let Some(method) = value["method"].as_str() else {
                peer.send(error(value["id"].clone(), -32600, "Invalid method"))
                    .await?;
                continue;
            };
            let params = value.get("params").cloned().unwrap_or_else(|| json!({}));
            if value.get("id").is_none() {
                if method == "session/cancel" {
                    let _ = server.handle(method, params).await;
                }
                continue;
            }
            if !(value["id"].is_string() || value["id"].is_number()) {
                peer.send(error(Value::Null, -32600, "Invalid JSON-RPC request ID"))
                    .await?;
                continue;
            }
            while handlers.try_join_next().is_some() {}
            if handlers.len() >= 2048 {
                peer.send(error(
                    value["id"].clone(),
                    -32000,
                    "Too many concurrent requests",
                ))
                .await?;
                continue;
            }
            let server = server.clone();
            let peer = peer.clone();
            let method = method.to_owned();
            handlers.spawn(async move {
                let id = value["id"].clone();
                let response = match server.handle(&method, params).await {
                    Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
                    Err(err) => {
                        let code = if err.to_string() == "Method not found" {
                            -32601
                        } else {
                            -32000
                        };
                        error(id, code, &format!("{err:#}"))
                    }
                };
                let _ = peer.send(response).await;
            });
            tokio::task::yield_now().await;
        }
        Ok(())
    }
    .await;
    server.shutdown().await;
    // Give cancelled turns time to checkpoint, but never wait indefinitely on a dead client.
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        while handlers.join_next().await.is_some() {}
    })
    .await;
    handlers.abort_all();
    while handlers.join_next().await.is_some() {}
    drop(server);
    drop(peer);
    let _ = tokio::time::timeout(Duration::from_secs(2), writer).await;
    result
}

fn error(id: Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        if let Ok(mut terminate) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}
