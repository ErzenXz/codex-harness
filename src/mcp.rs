use crate::{protocol::read_frame, provider::SseDecoder};
use anyhow::{Context, Result, bail, ensure};
use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex as StdMutex, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, Command},
    sync::{Mutex, oneshot},
};
use tokio_util::sync::CancellationToken;

const MAX_REPLY: usize = 1024 * 1024;
const MAX_TOOLS: usize = 32;
const TIMEOUT: Duration = Duration::from_secs(45);
type Pending = Arc<StdMutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;
type ToolCache = Mutex<Option<(Instant, u64, Vec<Value>)>>;
type Connection = Mutex<Option<Arc<Client>>>;

struct PendingRequest {
    requests: Pending,
    id: u64,
}
impl Drop for PendingRequest {
    fn drop(&mut self) {
        self.requests.lock().unwrap().remove(&self.id);
    }
}

#[derive(Clone)]
enum Location {
    Stdio {
        command: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
    },
    Http {
        url: reqwest::Url,
        headers: HeaderMap,
    },
}

#[derive(Clone)]
struct Definition {
    name: String,
    location: Location,
}

pub struct Target {
    client: Arc<Client>,
    name: String,
}

pub struct Tools {
    pub definitions: Vec<Value>,
    pub targets: HashMap<String, Target>,
}

pub struct Pool {
    entries: StdMutex<HashMap<String, Weak<Connection>>>,
    limit: usize,
}

impl Default for Pool {
    fn default() -> Self {
        Self::new(32)
    }
}
impl Pool {
    pub fn new(limit: usize) -> Self {
        Self {
            entries: StdMutex::new(HashMap::new()),
            limit,
        }
    }
}

#[derive(Clone)]
pub struct Servers {
    cwd: PathBuf,
    defs: Vec<(Definition, Arc<Connection>)>,
}

impl Servers {
    pub fn parse(value: &Value, cwd: &Path, pool: Arc<Pool>) -> Result<Self> {
        let entries = value.as_array().context("mcpServers must be an array")?;
        ensure!(entries.len() <= 8, "At most 8 MCP servers per session");
        let mut seen = HashSet::new();
        let mut defs = Vec::with_capacity(entries.len());
        for entry in entries {
            let name = entry["name"].as_str().context("MCP server missing name")?;
            ensure!(
                name.len() <= 24
                    && !name.is_empty()
                    && name
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-'),
                "Invalid MCP server name"
            );
            ensure!(seen.insert(name.to_owned()), "Duplicate MCP server name");
            let location = if let Some(command) = entry["command"].as_str() {
                ensure!(
                    entry.get("type").is_none_or(|t| t == "stdio"),
                    "Invalid MCP stdio type"
                );
                ensure!(
                    !command.is_empty() && command.len() <= 4096,
                    "Invalid MCP command"
                );
                let args = entry["args"]
                    .as_array()
                    .context("MCP args must be an array")?;
                ensure!(args.len() <= 64, "Too many MCP arguments");
                let args = args
                    .iter()
                    .map(|v| {
                        v.as_str()
                            .map(str::to_owned)
                            .context("Invalid MCP argument")
                    })
                    .collect::<Result<Vec<_>>>()?;
                ensure!(
                    args.iter().all(|arg| arg.len() <= 8192)
                        && args.iter().map(String::len).sum::<usize>() <= 65536,
                    "MCP arguments exceed limits"
                );
                let env = names(entry.get("env"), 32, "env")?;
                for (key, _) in &env {
                    ensure!(
                        !key.is_empty()
                            && key.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_'),
                        "Invalid MCP environment name"
                    );
                }
                Location::Stdio {
                    command: command.into(),
                    args,
                    env,
                }
            } else {
                ensure!(
                    entry["type"] == "http",
                    "Unsupported MCP transport (use stdio or Streamable HTTP)"
                );
                let url = reqwest::Url::parse(
                    entry["url"]
                        .as_str()
                        .context("MCP HTTP server missing url")?,
                )?;
                let local = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
                ensure!(
                    url.username().is_empty()
                        && url.password().is_none()
                        && url.fragment().is_none()
                        && (url.scheme() == "https" || (local && url.scheme() == "http")),
                    "MCP URL must use HTTPS (except loopback), without credentials or fragment"
                );
                let mut headers = HeaderMap::new();
                for (name, value) in names(entry.get("headers"), 32, "headers")? {
                    let key: HeaderName = name.parse()?;
                    ensure!(
                        ![
                            "host",
                            "accept",
                            "content-type",
                            "content-length",
                            "mcp-session-id",
                            "mcp-protocol-version"
                        ]
                        .contains(&key.as_str()),
                        "Reserved MCP HTTP header"
                    );
                    headers.insert(key, HeaderValue::from_str(&value)?);
                }
                Location::Http { url, headers }
            };
            // Credentials in env/headers remain in memory and are never emitted in diagnostics.
            let key = serde_json::to_string(&(cwd, entry))?;
            let cell = {
                let mut entries = pool.entries.lock().unwrap();
                entries.retain(|_, v| v.strong_count() > 0);
                if let Some(existing) = entries.get(&key).and_then(Weak::upgrade) {
                    existing
                } else {
                    ensure!(
                        entries.len() < pool.limit,
                        "Global MCP connection limit reached"
                    );
                    let cell = Arc::new(Mutex::new(None));
                    entries.insert(key.clone(), Arc::downgrade(&cell));
                    cell
                }
            };
            defs.push((
                Definition {
                    name: name.into(),
                    location,
                },
                cell,
            ));
        }
        Ok(Self {
            cwd: cwd.into(),
            defs,
        })
    }

    pub async fn tools(&self, cancel: &CancellationToken) -> Result<Tools> {
        let mut tools = Tools {
            definitions: Vec::new(),
            targets: HashMap::new(),
        };
        for (def, cell) in &self.defs {
            let client = {
                let mut connection = tokio::select! {
                    _ = cancel.cancelled() => bail!("MCP discovery cancelled"),
                    connection = cell.lock() => connection,
                };
                if let Some(client) = connection
                    .as_ref()
                    .filter(|client| client.alive.load(Ordering::Relaxed))
                {
                    client.clone()
                } else {
                    let client = Client::connect(&def.location, &self.cwd, cancel).await?;
                    *connection = Some(client.clone());
                    client
                }
            };
            for tool in client.list_tools(cancel).await? {
                ensure!(
                    tools.definitions.len() < MAX_TOOLS,
                    "Too many MCP tools (maximum 32 across all servers)"
                );
                let original = tool["name"].as_str().context("MCP tool missing name")?;
                ensure!(
                    original.len() <= 128
                        && !original.is_empty()
                        && original
                            .bytes()
                            .all(|c| c.is_ascii_alphanumeric() || b"_- .".contains(&c))
                        && !original.contains(' '),
                    "Invalid MCP tool name"
                );
                let schema = tool["inputSchema"]
                    .as_object()
                    .context("MCP tool missing inputSchema")?;
                ensure!(
                    serde_json::to_vec(&schema)?.len() <= 4096,
                    "MCP tool schema exceeds 4 KiB"
                );
                let description = tool["description"].as_str().unwrap_or("External MCP tool");
                let description = crate::workspace::truncate(description, 512);
                let proposed = format!("mcp_{}_{}", def.name, original.replace(['.', '-'], "_"));
                let mut hash = 0xcbf29ce484222325_u64;
                for byte in def.name.bytes().chain([0]).chain(original.bytes()) {
                    hash = (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
                }
                let name = format!("{}_{hash:016x}", &proposed[..proposed.len().min(47)]);
                ensure!(
                    !tools.targets.contains_key(&name),
                    "MCP tool name collision"
                );
                tools.definitions.push(json!({"type":"function","name":name,"description":format!("MCP server {}: {description}. External access requires approval.",def.name),"parameters":schema,"strict":false}));
                tools.targets.insert(
                    name,
                    Target {
                        client: client.clone(),
                        name: original.into(),
                    },
                );
            }
        }
        Ok(tools)
    }
}

fn names(value: Option<&Value>, max: usize, field: &str) -> Result<Vec<(String, String)>> {
    let Some(value) = value else {
        return Ok(vec![]);
    };
    let values = value
        .as_array()
        .with_context(|| format!("MCP {field} must be an array"))?;
    ensure!(values.len() <= max, "Too many MCP {field} entries");
    let mut seen = HashSet::new();
    values
        .iter()
        .map(|v| {
            let key = v["name"].as_str().context("MCP entry missing name")?;
            let val = v["value"].as_str().context("MCP entry missing value")?;
            ensure!(
                seen.insert(key.to_ascii_lowercase()),
                "Duplicate MCP {field} entry"
            );
            ensure!(key.len() <= 128 && val.len() <= 8192, "MCP entry too large");
            Ok((key.into(), val.into()))
        })
        .collect()
}

impl Target {
    pub async fn call(&self, args: &Value, cancel: &CancellationToken) -> Result<Value> {
        ensure!(args.is_object(), "MCP arguments must be an object");
        ensure!(
            serde_json::to_vec(args)?.len() <= 65536,
            "MCP arguments exceed 64 KiB"
        );
        let result = self
            .client
            .request(
                "tools/call",
                json!({"name":self.name,"arguments":args}),
                cancel,
            )
            .await?;
        ensure!(
            result["content"].is_array() || result["structuredContent"].is_object(),
            "Invalid MCP tool result"
        );
        Ok(result)
    }
}

struct Client {
    transport: Transport,
    sequence: AtomicU64,
    tools: ToolCache,
    generation: Arc<AtomicU64>,
    has_tools: bool,
    alive: Arc<AtomicBool>,
}

enum Transport {
    Stdio {
        writer: Arc<Mutex<ChildStdin>>,
        pending: Pending,
        child: Child,
        reader: tokio::task::JoinHandle<()>,
        group: crate::jobs::ProcessGroup,
    },
    Http {
        http: reqwest::Client,
        url: reqwest::Url,
        headers: HeaderMap,
        session: Mutex<Option<String>>,
        version: Mutex<String>,
    },
}

impl Drop for Client {
    fn drop(&mut self) {
        if let Transport::Stdio {
            child,
            reader,
            group,
            ..
        } = &mut self.transport
        {
            reader.abort();
            group.kill();
            let _ = child.start_kill();
        }
    }
}

impl Client {
    async fn connect(
        location: &Location,
        cwd: &Path,
        cancel: &CancellationToken,
    ) -> Result<Arc<Self>> {
        let tools = Mutex::new(None);
        let generation = Arc::new(AtomicU64::new(0));
        let alive = Arc::new(AtomicBool::new(true));
        let transport = match location {
            Location::Stdio { command, args, env } => {
                let mut cmd = Command::new(command);
                cmd.args(args)
                    .current_dir(cwd)
                    .kill_on_drop(true)
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::null());
                cmd.env_clear();
                crate::jobs::inherit_process_environment(&mut cmd);
                for (name, value) in env {
                    cmd.env(name, value);
                }
                #[cfg(unix)]
                {
                    use std::os::unix::process::CommandExt;
                    cmd.as_std_mut().process_group(0);
                }
                let mut child = cmd.spawn().context("Could not start MCP stdio server")?;
                let group = crate::jobs::ProcessGroup::new(&mut child)?;
                let stdin = child.stdin.take().context("Missing MCP stdin")?;
                let stdout = child.stdout.take().context("Missing MCP stdout")?;
                let writer = Arc::new(Mutex::new(stdin));
                let pending: Pending = Arc::new(StdMutex::new(HashMap::new()));
                let reader_pending = pending.clone();
                let reader_writer = writer.clone();
                let reader_generation = generation.clone();
                let reader_alive = alive.clone();
                let reader = tokio::spawn(async move {
                    let mut reader = BufReader::new(stdout);
                    let failure = loop {
                        let line = match read_frame(&mut reader).await {
                            Ok(Some(line)) if line.len() <= MAX_REPLY => line,
                            Ok(Some(_)) => break "MCP response exceeds 1 MiB",
                            Ok(None) => break "MCP stdio server exited",
                            Err(_) => break "Invalid MCP stdio frame",
                        };
                        let Ok(frame) = serde_json::from_slice::<Value>(&line) else {
                            break "Invalid MCP JSON";
                        };
                        if frame.get("id").is_some() && frame["method"].is_string() {
                            let id = &frame["id"];
                            let result = if frame["method"] == "ping" {
                                json!({"jsonrpc":"2.0","id":id,"result":{}})
                            } else {
                                json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Unsupported client method"}})
                            };
                            if !matches!(
                                tokio::time::timeout(
                                    Duration::from_secs(5),
                                    Self::send_stdio(
                                        &reader_writer,
                                        &result,
                                        &CancellationToken::new()
                                    )
                                )
                                .await,
                                Ok(Ok(()))
                            ) {
                                break "MCP stdio write failed";
                            }
                        } else if let Some(id) = frame["id"].as_u64() {
                            if let Some(tx) = reader_pending.lock().unwrap().remove(&id) {
                                let _ = tx.send(Ok(frame));
                            }
                        } else if frame["method"] == "notifications/tools/list_changed" {
                            reader_generation.fetch_add(1, Ordering::Relaxed);
                        }
                    };
                    reader_alive.store(false, Ordering::Relaxed);
                    for (_, tx) in reader_pending.lock().unwrap().drain() {
                        let _ = tx.send(Err(failure.into()));
                    }
                });
                Transport::Stdio {
                    writer,
                    pending,
                    group,
                    child,
                    reader,
                }
            }
            Location::Http { url, headers } => Transport::Http {
                http: reqwest::Client::builder()
                    .connect_timeout(Duration::from_secs(10))
                    .redirect(reqwest::redirect::Policy::none())
                    .build()?,
                url: url.clone(),
                headers: headers.clone(),
                session: Mutex::new(None),
                version: Mutex::new("2025-11-25".into()),
            },
        };
        let mut client = Self {
            transport,
            sequence: AtomicU64::new(1),
            tools,
            generation,
            has_tools: false,
            alive,
        };
        let result = client.request("initialize",json!({"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"piko","version":env!("CARGO_PKG_VERSION")}}),cancel).await?;
        let version = result["protocolVersion"]
            .as_str()
            .context("MCP server omitted protocolVersion")?;
        ensure!(
            ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"].contains(&version),
            "Unsupported MCP protocol version"
        );
        client.has_tools = result["capabilities"]["tools"].is_object();
        if let Transport::Http {
            version: negotiated,
            ..
        } = &client.transport
        {
            *negotiated.lock().await = version.into();
        }
        client
            .notify("notifications/initialized", json!({}), cancel)
            .await?;
        Ok(Arc::new(client))
    }

    async fn list_tools(&self, cancel: &CancellationToken) -> Result<Vec<Value>> {
        if !self.has_tools {
            return Ok(Vec::new());
        }
        let mut cached = tokio::select! {
            _ = cancel.cancelled() => bail!("MCP discovery cancelled"),
            cached = self.tools.lock() => cached,
        };
        let generation = self.generation.load(Ordering::Relaxed);
        if let Some((when, version, tools)) = cached.as_ref()
            && when.elapsed() < Duration::from_secs(60)
            && *version == generation
        {
            return Ok(tools.clone());
        }
        let mut cursor: Option<String> = None;
        let mut seen = HashSet::new();
        let mut tools = Vec::new();
        loop {
            let params = cursor
                .as_ref()
                .map_or_else(|| json!({}), |c| json!({"cursor":c}));
            let result = self.request("tools/list", params, cancel).await?;
            let page = result["tools"]
                .as_array()
                .context("MCP tools/list missing tools array")?;
            ensure!(
                tools.len() + page.len() <= MAX_TOOLS,
                "MCP server has more than 32 tools"
            );
            tools.extend(page.iter().cloned());
            let Some(next) = result["nextCursor"].as_str() else {
                break;
            };
            ensure!(
                next.len() <= 256 && seen.len() < 32 && seen.insert(next.to_owned()),
                "Invalid MCP pagination cursor"
            );
            cursor = Some(next.into());
        }
        tools.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        *cached = Some((Instant::now(), generation, tools.clone()));
        Ok(tools)
    }

    async fn notify(&self, method: &str, params: Value, cancel: &CancellationToken) -> Result<()> {
        let frame = json!({"jsonrpc":"2.0","method":method,"params":params});
        match &self.transport {
            Transport::Stdio { writer, .. } => Self::send_stdio(writer, &frame, cancel).await,
            Transport::Http {
                http,
                url,
                headers,
                session,
                version,
            } => {
                let session_id = session.lock().await.clone();
                let protocol = version.lock().await.clone();
                let response = Self::post(
                    http,
                    url,
                    headers,
                    session_id.as_deref(),
                    &protocol,
                    &frame,
                    cancel,
                )
                .await?;
                ensure!(
                    response.status() == reqwest::StatusCode::ACCEPTED,
                    "MCP notification was rejected"
                );
                Ok(())
            }
        }
    }

    async fn request(
        &self,
        method: &str,
        params: Value,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        let id = self.sequence.fetch_add(1, Ordering::Relaxed);
        let frame = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        let result=async {
        let reply = match &self.transport {
            Transport::Stdio {
                writer, pending, ..
            } => {
                let (tx, rx) = oneshot::channel();
                {
                    let mut pending = pending.lock().unwrap();
                    ensure!(pending.len() < 64, "Too many concurrent MCP requests");
                    pending.insert(id, tx);
                }
                let _pending=PendingRequest {requests:pending.clone(),id};
                let result = async {
                    Self::send_stdio(writer,&frame,cancel).await?;
                    tokio::select! {
                        _ = cancel.cancelled() => bail!("MCP request cancelled"),
                        result = tokio::time::timeout(TIMEOUT,rx) => result.context("MCP timed out")?.context("MCP server disconnected")?.map_err(anyhow::Error::msg),
                    }
                }.await;
                if result.is_err() && method!="initialize" {
                    pending.lock().unwrap().remove(&id);
                    let notification = json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":id,"reason":"Client stopped waiting"}});
                    let _ = tokio::time::timeout(
                        Duration::from_millis(200),
                        Self::send_stdio(writer, &notification, &CancellationToken::new()),
                    )
                    .await;
                }
                result?
            }
            Transport::Http {
                http,
                url,
                headers,
                session,
                version,
            } => {
                let session_id = session.lock().await.clone();
                let protocol = version.lock().await.clone();
                let response = Self::post(
                    http,
                    url,
                    headers,
                    session_id.as_deref(),
                    &protocol,
                    &frame,
                    cancel,
                )
                .await?;
                if method == "initialize"
                    && let Some(value) = response.headers().get("mcp-session-id")
                {
                    *session.lock().await = Some(value.to_str()?.into());
                }
                if response.status() == reqwest::StatusCode::NOT_FOUND {
                    self.alive.store(false, Ordering::Relaxed);
                }
                ensure!(
                    response.status().is_success(),
                    "MCP HTTP request failed with {}",
                    response.status()
                );
                let mime = response
                    .headers()
                    .get("content-type")
                    .and_then(|h| h.to_str().ok())
                    .unwrap_or("")
                    .split(';')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_owned();
                let mut body = response.bytes_stream();
                let mut bytes = Vec::new();
                let mut total = 0;
                let mut decoder = SseDecoder::with_limit(MAX_REPLY);
                let stream = async {
                    while let Some(chunk) = body.next().await {
                        let chunk = chunk.map_err(|_|anyhow::anyhow!("MCP HTTP body failed"))?;
                        total += chunk.len();
                        ensure!(total <= MAX_REPLY, "MCP response exceeds 1 MiB");
                        if mime == "text/event-stream" {
                            for data in decoder.push(&chunk)? {
                                let value: Value =
                                    serde_json::from_str(&data).context("Invalid MCP SSE JSON")?;
                                if value["method"].is_string() && value.get("id").is_some() {
                                    let answer = if value["method"] == "ping" {
                                        json!({"jsonrpc":"2.0","id":value["id"],"result":{}})
                                    } else {
                                        json!({"jsonrpc":"2.0","id":value["id"],"error":{"code":-32601,"message":"Unsupported client method"}})
                                    };
                                    Self::post(
                                        http,
                                        url,
                                        headers,
                                        session_id.as_deref(),
                                        &protocol,
                                        &answer,
                                        cancel,
                                    )
                                    .await?;
                                } else if value["method"] == "notifications/tools/list_changed" {
                                    self.generation.fetch_add(1, Ordering::Relaxed);
                                } else if value["id"] == id {
                                    return Ok(value);
                                }
                            }
                        } else {
                            bytes.extend_from_slice(&chunk);
                        }
                    }
                    ensure!(
                        mime == "application/json",
                        "Unsupported MCP HTTP response MIME"
                    );
                    Ok(serde_json::from_slice::<Value>(&bytes)?)
                };
                tokio::select! {
                    _ = cancel.cancelled() => bail!("MCP request cancelled"),
                    value = tokio::time::timeout(TIMEOUT,stream) => value.context("MCP HTTP timed out")??,
                }
            }
        };
        ensure!(
            reply["jsonrpc"] == "2.0" && reply["id"] == id,
            "MCP response ID mismatch"
        );
        ensure!(reply.get("error").is_none(), "MCP server returned an error");
        ensure!(reply.get("result").is_some(), "MCP response missing result");
        Ok(reply["result"].clone())
        }.await;
        if result.is_err()
            && method != "initialize"
            && matches!(self.transport, Transport::Http { .. })
        {
            let _ = tokio::time::timeout(
                Duration::from_millis(200),
                self.notify(
                    "notifications/cancelled",
                    json!({"requestId":id,"reason":"Client stopped waiting"}),
                    &CancellationToken::new(),
                ),
            )
            .await;
        }
        result
    }

    async fn send_stdio(
        writer: &Mutex<ChildStdin>,
        frame: &Value,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let bytes = serde_json::to_vec(frame)?;
        ensure!(bytes.len() <= 65536, "MCP request exceeds 64 KiB");
        let mut writer = tokio::select! {
            _ = cancel.cancelled() => bail!("MCP request cancelled"),
            writer = tokio::time::timeout(TIMEOUT,writer.lock()) => writer.context("MCP write queue timed out")?,
        };
        tokio::select! {
            _ = cancel.cancelled() => bail!("MCP request cancelled"),
            value = tokio::time::timeout(TIMEOUT,async { writer.write_all(&bytes).await?; writer.write_all(b"\n").await?; writer.flush().await }) => value.context("MCP stdio write timed out")??,
        }
        Ok(())
    }

    async fn post(
        http: &reqwest::Client,
        url: &reqwest::Url,
        headers: &HeaderMap,
        session: Option<&str>,
        version: &str,
        frame: &Value,
        cancel: &CancellationToken,
    ) -> Result<reqwest::Response> {
        let mut request = http
            .post(url.clone())
            .headers(headers.clone())
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", version)
            .json(frame);
        ensure!(
            serde_json::to_vec(frame)?.len() <= 65536,
            "MCP request exceeds 64 KiB"
        );
        if let Some(session) = session {
            request = request.header("Mcp-Session-Id", session);
        }
        tokio::select! {
            _ = cancel.cancelled() => bail!("MCP request cancelled"),
            response = tokio::time::timeout(TIMEOUT,request.send()) => response.context("MCP HTTP timed out")?.map_err(|_| anyhow::anyhow!("MCP HTTP transport failed")),
        }
    }
}
