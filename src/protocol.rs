use crate::{
    agent::AgentContext,
    config::Config,
    jobs::Jobs,
    mcp::{Pool as McpPool, Servers as McpServers},
    models::{ModelCatalog, ModelInfo},
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

#[derive(Debug)]
struct InvalidParams(&'static str);

impl std::fmt::Display for InvalidParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for InvalidParams {}

fn selection<'a>(params: &'a Value, key: &str) -> Result<&'a str> {
    params[key]
        .as_str()
        .filter(|value| {
            !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
        })
        .ok_or_else(|| InvalidParams("Missing or invalid selection parameter").into())
}

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
    mailboxes: Arc<crate::agents::Mailboxes>,
    mcp: Arc<McpServers>,
    models: ModelCatalog,
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
    builtins: Arc<crate::agent::Builtins>,
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
            builtins: Arc::new(crate::agent::Builtins::default()),
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
                json!({"protocolVersion":1,"agentInfo":{"name":"piko","version":env!("CARGO_PKG_VERSION"),"title":"Piko"},
                "agentCapabilities":{"loadSession":true,"promptCapabilities":{},"mcpCapabilities":{"http":true,"sse":false},"_meta":{"harness":{"tasks":true,"boundedContext":true,"computerUse":self.config.computer_use && cfg!(target_os = "linux")}}},
                "authMethods":[{"id":"chatgpt","name":"ChatGPT subscription","description":"Run piko login before starting the agent"},{"id":"api_key","name":"Provider API key","description":"Set the configured api_key_env environment variable"}]}),
            );
        }
        ensure!(
            self.initialized.load(Ordering::Acquire),
            "Call initialize before session methods"
        );
        match method {
            "_harness/stats" => Ok(self.provider.metrics.snapshot()),
            "_harness/models" => Ok(json!({"models":self.provider.models().await})),
            "authenticate" => {
                let method = string(&params, "methodId")?;
                ensure!(
                    ["chatgpt", "api_key"].contains(&method),
                    "Unknown authentication method"
                );
                let sessions = self.sessions.lock().await;
                ensure!(
                    sessions.is_empty(),
                    "Unload all sessions before changing authentication"
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
                let mut session = if method == "session/load" {
                    self.store
                        .load(Uuid::parse_str(string(&params, "sessionId")?)?, &workspace)
                        .await?
                } else {
                    Session::new(&workspace, &self.config)
                };
                // Discovery may perform network I/O; never hold the session registry lock.
                let models = self.provider.models().await;
                if session.version == 1 {
                    session.reasoning_effort = self.config.reasoning_effort.clone();
                    session.version = 2;
                }
                if let Some(model) = models.iter().find(|model| model.id == session.model)
                    && (session.reasoning_effort.as_deref().is_some_and(|effort| {
                        (model.reasoning_capabilities_known || !model.reasoning_efforts.is_empty())
                            && !self.effort_allowed(model, effort)
                    }) || (method == "session/new" && session.reasoning_effort.is_none()))
                {
                    session.reasoning_effort = model.default_reasoning_effort.clone();
                }
                let id = session.id;
                let slot = Arc::new(Slot {
                    session: Mutex::new(session),
                    workspace,
                    busy: Arc::new(Semaphore::new(0)),
                    cancel: Mutex::new(self.shutdown.child_token()),
                    jobs: Jobs::new(self.config.max_jobs, self.config.tool_output_bytes),
                    mailboxes: Arc::new(crate::agents::Mailboxes::default()),
                    mcp,
                    models,
                });
                let mut sessions = self.sessions.lock().await;
                ensure!(
                    sessions.len() < self.config.max_sessions,
                    "Loaded session limit reached; unload a session first"
                );
                ensure!(!sessions.contains_key(&id), "Session already loaded");
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
                let session = slot.session.lock().await;
                Ok(json!({"sessionId":id,"modes":modes(&session.mode),
                    "models":{"currentModelId":session.model,"availableModels":self.available_models(&session,&slot.models)},
                    "configOptions":self.config_options(&session,&slot.models)}))
            }
            "session/prompt" => self.prompt(params).await,
            "session/cancel" => {
                let slot = self.slot(&params).await?;
                slot.cancel.lock().await.cancel();
                Ok(json!({}))
            }
            "session/set_model" => {
                self.set_option(&params, "model", "modelId").await?;
                Ok(json!({}))
            }
            "session/set_mode" => {
                self.set_option(&params, "mode", "modeId").await?;
                Ok(json!({}))
            }
            "session/set_config_option" => {
                let config_id = selection(&params, "configId")?;
                self.set_option(&params, config_id, "value").await
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

    fn effort_allowed(&self, model: &ModelInfo, effort: &str) -> bool {
        model
            .reasoning_efforts
            .iter()
            .any(|option| option.effort == effort)
            || (!model.reasoning_capabilities_known
                && model.reasoning_efforts.is_empty()
                && model.id == self.config.model
                && self.config.reasoning_effort.as_deref() == Some(effort))
    }

    fn reasoning_options(&self, model: &ModelInfo, saved: Option<&str>) -> Vec<Value> {
        let mut options = vec![
            json!({"value":"default","name":"Provider default","description":"Let the provider choose its default reasoning effort"}),
        ];
        for option in &model.reasoning_efforts {
            if option.effort != "default" {
                options.push(json!({"value":option.effort,"name":effort_name(&option.effort),"description":short(&option.description,256)}));
            }
        }
        if !model.reasoning_capabilities_known
            && model.reasoning_efforts.is_empty()
            && model.id == self.config.model
            && let Some(effort) = &self.config.reasoning_effort
        {
            options.push(json!({"value":effort,"name":effort_name(effort),"description":"From your trusted provider configuration"}));
        }
        if !model.reasoning_capabilities_known
            && model.reasoning_efforts.is_empty()
            && let Some(effort) = saved
            && effort != "default"
            && !options.iter().any(|option| option["value"] == effort)
        {
            options.push(json!({"value":effort,"name":effort_name(effort),"description":"Saved setting; provider capabilities are unavailable"}));
        }
        options
    }

    fn available_models(&self, session: &Session, catalog: &[ModelInfo]) -> Vec<Value> {
        let mut models: Vec<Value> = catalog.iter().map(|model| {
            let efforts: Vec<Value> = self.reasoning_options(model, if model.id == session.model { session.reasoning_effort.as_deref() } else { None }).into_iter().map(|value| value["value"].clone()).collect();
            let mut value = json!({"modelId":model.id,"name":short(&model.name,128),"_meta":{"harness":{"reasoningEfforts":efforts}}});
            if let Some(description) = &model.description {
                value["description"] = json!(short(description,256));
            }
            value
        }).collect();
        if !catalog.iter().any(|model| model.id == session.model) {
            let fallback = ModelInfo::configured(&session.model);
            let efforts: Vec<Value> = self
                .reasoning_options(&fallback, session.reasoning_effort.as_deref())
                .into_iter()
                .map(|option| option["value"].clone())
                .collect();
            models.push(json!({"modelId":session.model,"name":short(&session.model,128),"description":"Model retained by this saved session","_meta":{"harness":{"reasoningEfforts":efforts}}}));
        }
        models
    }

    fn config_options(&self, session: &Session, catalog: &[ModelInfo]) -> Value {
        let options: Vec<Value> = self
            .available_models(session, catalog)
            .into_iter()
            .map(|model| {
                let mut option =
                    json!({"value":model["modelId"],"name":model["name"],"_meta":model["_meta"]});
                if model["description"].is_string() {
                    option["description"] = model["description"].clone();
                }
                option
            })
            .collect();
        let fallback = ModelInfo::configured(&session.model);
        let model = catalog
            .iter()
            .find(|model| model.id == session.model)
            .unwrap_or(&fallback);
        json!([
            {"id":"model","name":"Model","category":"model","type":"select","currentValue":session.model,"options":options},
            {"id":"reasoning_effort","name":"Reasoning effort","category":"thought_level","type":"select","currentValue":session.reasoning_effort.as_deref().unwrap_or("default"),"options":self.reasoning_options(model, session.reasoning_effort.as_deref())},
            {"id":"mode","name":"Session mode","category":"mode","type":"select","currentValue":session.mode,"options":[
                {"value":"code","name":"Code","description":"Explore and edit; writes and shell commands require approval"},
                {"value":"plan","name":"Plan","description":"Read-only exploration; writes and shell commands disabled"}
            ]}
        ])
    }

    async fn set_option(&self, params: &Value, config_id: &str, value_key: &str) -> Result<Value> {
        let value = selection(params, value_key)?;
        let slot = self.slot(params).await?;
        let _permit = slot
            .busy
            .clone()
            .try_acquire_owned()
            .context("Session is busy")?;
        let mut session = slot.session.lock().await;
        let previous = (
            session.model.clone(),
            session.reasoning_effort.clone(),
            session.mode.clone(),
        );
        match config_id {
            "model" => {
                if value != session.model {
                    let model = slot
                        .models
                        .iter()
                        .find(|model| model.id == value)
                        .ok_or(InvalidParams("Model is not available in this session"))?;
                    session.reasoning_effort = session
                        .reasoning_effort
                        .take()
                        .filter(|effort| self.effort_allowed(model, effort))
                        .or_else(|| model.default_reasoning_effort.clone());
                    session.model = value.to_owned();
                }
            }
            "reasoning_effort" => {
                if value == "default" {
                    session.reasoning_effort = None;
                } else {
                    let fallback = ModelInfo::configured(&session.model);
                    let model = slot
                        .models
                        .iter()
                        .find(|model| model.id == session.model)
                        .unwrap_or(&fallback);
                    let saved_unknown_effort = !model.reasoning_capabilities_known
                        && model.reasoning_efforts.is_empty()
                        && session.reasoning_effort.as_deref() == Some(value);
                    if !self.effort_allowed(model, value) && !saved_unknown_effort {
                        return Err(InvalidParams(
                            "Reasoning effort is not available for the selected model",
                        )
                        .into());
                    }
                    session.reasoning_effort = Some(value.to_owned());
                }
            }
            "mode" => {
                if !["code", "plan"].contains(&value) {
                    return Err(InvalidParams("Unknown session mode").into());
                }
                if value != session.mode {
                    ensure!(
                        !slot
                            .jobs
                            .list()
                            .await
                            .iter()
                            .any(|job| job["status"] == "running"),
                        "Cancel running background tasks before changing mode"
                    );
                    session.mode = value.to_owned();
                }
            }
            _ => return Err(InvalidParams("Unknown configuration option").into()),
        }
        // Clients may reselect the same model before every prompt. Avoid rewriting
        // the full persisted history or emitting duplicate updates for a no-op.
        if previous.0 == session.model
            && previous.1 == session.reasoning_effort
            && previous.2 == session.mode
        {
            return Ok(json!({"configOptions":self.config_options(&session,&slot.models)}));
        }
        if let Err(error) = self.store.save(&session).await {
            (session.model, session.reasoning_effort, session.mode) = previous;
            return Err(error);
        }
        if previous.2 != session.mode {
            self.peer
                .update(
                    &session.id.to_string(),
                    json!({"sessionUpdate":"current_mode_update","currentModeId":session.mode}),
                )
                .await?;
        }
        let config = self.config_options(&session, &slot.models);
        self.peer
            .update(
                &session.id.to_string(),
                json!({"sessionUpdate":"config_option_update","configOptions":config}),
            )
            .await?;
        Ok(json!({"configOptions":config}))
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
            mailboxes: slot.mailboxes.clone(),
            builtins: self.builtins.clone(),
            mcp: slot.mcp.clone(),
            desktop: self.desktop.clone(),
            agents: self.agents.clone(),
            searches: self.searches.clone(),
            session_id: session.id.to_string(),
            agent_id: None,
            model: session.model.clone(),
            reasoning_effort: session.reasoning_effort.clone(),
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
            + toolset.schemas.encoded.get().len()
            + 512;
        ensure!(
            reserved <= self.config.context_bytes,
            "Prompt and project instructions exceed context budget"
        );
        let (mut items, omitted) =
            session.context_with_omitted(self.config.context_bytes - reserved);
        if omitted + session.dropped_turns > 0 {
            items.insert(0,json!({"role":"developer","content":format!("{} older turns are outside the model window. Do not assume their contents; re-read the workspace as needed. Saved history is separate from this window.",omitted + session.dropped_turns)}));
        }
        let start = items.len();
        items.push(user);
        let base_bytes = crate::session::json_bytes(&*session)?;
        // Check capacity before spending a request. Durable intent is written
        // before the first tool boundary; read-only replies need one final save.
        ensure!(
            base_bytes
                .saturating_add(crate::session::json_bytes(&items[start..])?)
                .saturating_add(1024)
                <= crate::session::MAX_SESSION_BYTES as usize,
            "Session reached its 16MiB persistence limit; start a new session. Saved history is preserved."
        );
        let result = crate::agent::run_with_instructions(
            &ctx,
            &mut items,
            instructions,
            toolset,
            Some(crate::agent::Checkpoint {
                store: &self.store,
                session: &session,
                start,
                base_bytes,
            }),
        )
        .await;
        // Save complete exchanges only; interrupted responses never enter model history.
        let mut turn = items.split_off(start);
        repair_calls(&mut turn);
        session.turns.push_back(Turn { items: turn });
        let saved = self.store.save(&session).await;
        *slot.cancel.lock().await = self.shutdown.child_token();
        if let Err(error) = saved {
            // Never retain an unbounded unsaved turn, or overwrite its recovery
            // checkpoint with a later prompt. Reload explicitly after a save failure.
            session.turns.pop_back();
            self.sessions.lock().await.remove(&session.id);
            slot.jobs.cancel_all().await;
            return Err(
                error.context("Could not commit session; reload it to recover the last checkpoint")
            );
        }
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

pub(crate) fn unanswered_outputs(items: &[Value]) -> Vec<Value> {
    let mut pending = Vec::new();
    // Match in sequence: compatible providers may reuse an ID in a later step.
    // An older result must not hide a newly interrupted call with the same ID.
    for item in items {
        if let Some(id) = item["call_id"].as_str() {
            if item["type"] == "function_call" {
                pending.push(id);
            } else if item["type"] == "function_call_output"
                && let Some(index) = pending.iter().position(|pending| *pending == id)
            {
                pending.remove(index);
            }
        }
    }
    pending.into_iter().map(|id| json!({"type":"function_call_output","call_id":id,"output":json!({"error":"Tool was interrupted and not executed to completion. Inspect current state before retrying."}).to_string()})).collect()
}

pub fn repair_calls(items: &mut Vec<Value>) {
    items.extend(unanswered_outputs(items));
}

fn short(text: &str, limit: usize) -> String {
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

fn effort_name(effort: &str) -> String {
    match effort {
        "none" => "None".into(),
        "minimal" => "Minimal".into(),
        "low" => "Low".into(),
        "medium" => "Medium".into(),
        "high" => "High".into(),
        "xhigh" => "Extra high".into(),
        "max" => "Max".into(),
        "ultra" => "Ultra".into(),
        _ => effort.into(),
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
                        } else if err.downcast_ref::<InvalidParams>().is_some() {
                            -32602
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::Notify,
    };

    #[tokio::test]
    async fn slow_model_discovery_does_not_block_session_cancellation() {
        let temp = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let fixture = tokio::spawn({
            let started = started.clone();
            let release = release.clone();
            async move {
                for index in 0..2 {
                    let (stream, _) = listener.accept().await.unwrap();
                    let mut stream = BufReader::new(stream);
                    let mut line = String::new();
                    loop {
                        line.clear();
                        stream.read_line(&mut line).await.unwrap();
                        if line == "\r\n" {
                            break;
                        }
                    }
                    if index == 1 {
                        started.notify_one();
                        release.notified().await;
                    }
                    stream.get_mut().write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
                }
            }
        });
        let config = Config {
            auth: "none".into(),
            base_url: format!("http://{address}/v1"),
            state_dir: temp.path().join("state"),
            ..Config::default()
        };
        let (peer, _outbound) = Peer::channel(16);
        let server = Arc::new(Server::new(config, peer).await.unwrap());
        server
            .handle("initialize", json!({"protocolVersion":1}))
            .await
            .unwrap();
        let params = json!({"cwd":temp.path(), "mcpServers":[]});
        let first = server.handle("session/new", params.clone()).await.unwrap();
        server.provider.invalidate_models().await;
        let creation = tokio::spawn({
            let server = server.clone();
            async move { server.handle("session/new", params).await }
        });
        tokio::time::timeout(Duration::from_secs(1), started.notified())
            .await
            .unwrap();
        let cancelled = tokio::time::timeout(
            Duration::from_millis(250),
            server.handle("session/cancel", json!({"sessionId":first["sessionId"]})),
        )
        .await;
        release.notify_one();
        assert!(cancelled.unwrap().is_ok());
        assert!(creation.await.unwrap().is_ok());
        fixture.await.unwrap();
    }
}
