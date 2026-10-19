use crate::{auth::Auth, config::Config, metrics::Metrics, protocol::Peer};
use anyhow::{Context, Result, bail, ensure};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

const MAX_EVENT: usize = 4 * 1024 * 1024;

#[derive(Clone)]
pub struct Provider {
    client: reqwest::Client,
    config: Config,
    auth: Auth,
    requests: Arc<Semaphore>,
    pub metrics: Arc<Metrics>,
}

pub struct Response {
    pub output: Vec<Value>,
    pub text: String,
    pub incomplete: bool,
}

impl Provider {
    pub async fn authenticate(&self, method: &str) -> Result<()> {
        self.auth.authenticate(&self.client, method).await
    }

    pub fn new(config: &Config) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(20))
                .timeout(Duration::from_secs(config.request_timeout_secs))
                .redirect(reqwest::redirect::Policy::none())
                .pool_max_idle_per_host(config.max_model_requests.min(256))
                .build()?,
            config: config.clone(),
            auth: Auth::new(config),
            requests: Arc::new(Semaphore::new(config.max_model_requests)),
            metrics: Arc::new(Metrics::default()),
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn respond(
        &self,
        model: &str,
        instructions: &str,
        input: &[Value],
        tools: &[Value],
        peer: &Peer,
        session_id: &str,
        agent_id: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<Response> {
        let _permit = tokio::select! {
            _ = cancel.cancelled() => bail!("Cancelled"),
            permit = self.requests.acquire() => permit?,
        };
        let chat = self.config.wire_api == "chat_completions";
        let mut credentials = self.auth.credentials(&self.client, false).await?;
        let mut body = json!({
            "model": model, "instructions": instructions, "input": input, "tools": tools,
            "stream": true, "store": false, "parallel_tool_calls": true,
            "max_output_tokens": self.config.max_output_tokens,
            "include": ["reasoning.encrypted_content"]
        });
        if let Some(effort) = &self.config.reasoning_effort {
            body["reasoning"] = json!({"effort": effort});
        }
        if credentials.account_id.is_some() {
            body.as_object_mut()
                .expect("object")
                .remove("max_output_tokens");
            body["prompt_cache_key"] = json!(session_id);
        }
        if chat {
            body = chat_request(
                model,
                instructions,
                input,
                tools,
                self.config.max_output_tokens,
            )?;
        }
        if credentials.account_id.is_some()
            || credentials.base_url.trim_end_matches('/') == "https://api.openai.com/v1"
        {
            body["prompt_cache_key"] = json!(format!("harness-v1:{session_id}"));
            if credentials.account_id.is_none()
                && let Some(retention) = &self.config.prompt_cache_retention
            {
                body["prompt_cache_retention"] = json!(retention);
            }
        }
        let mut attempt = 0;
        let mut refreshed = false;
        let response = loop {
            let endpoint = if chat {
                "chat/completions"
            } else {
                "responses"
            };
            let mut request = self
                .client
                .post(format!(
                    "{}/{endpoint}",
                    credentials.base_url.trim_end_matches('/')
                ))
                .json(&body);
            if let Some(key) = &credentials.bearer {
                request = request.bearer_auth(key);
            }
            if let Some(account) = &credentials.account_id {
                request = request
                    .header("ChatGPT-Account-Id", account)
                    .header("originator", "codex_cli_rs");
            }
            let response = tokio::select! {
                biased;
                _ = cancel.cancelled() => bail!("Cancelled"),
                response = request.send() => response.context("Model request failed")?,
            };
            if response.status().is_success() {
                break response;
            }
            let status = response.status();
            if status.as_u16() == 401 && credentials.account_id.is_some() && !refreshed {
                credentials = self.auth.credentials(&self.client, true).await?;
                refreshed = true;
                continue;
            }
            if (status.as_u16() == 429 || status.is_server_error()) && attempt < 3 {
                let delay = response
                    .headers()
                    .get("retry-after")
                    .and_then(|h| h.to_str().ok())
                    .and_then(|h| h.parse::<u64>().ok())
                    .unwrap_or(1 << attempt)
                    .min(30);
                attempt += 1;
                tokio::select! {
                    _ = cancel.cancelled() => bail!("Cancelled"),
                    _ = tokio::time::sleep(Duration::from_secs(delay)) => {}
                }
                continue;
            }
            // Do not forward arbitrary upstream bodies: they may echo sensitive input.
            bail!(
                "Provider returned HTTP {} (check credentials, model, quota, and endpoint)",
                status.as_u16()
            );
        };
        ensure!(
            response
                .headers()
                .get("content-type")
                .and_then(|h| h.to_str().ok())
                .is_some_and(|h| h.starts_with("text/event-stream")),
            "Provider did not return an SSE stream"
        );
        let mut stream = response.bytes_stream();
        let mut decoder = SseDecoder::default();
        let mut text = String::new();
        let mut chat_stream = ChatStream::default();
        let mut chat_usage = None;
        while let Some(chunk) = tokio::select! {
            biased;
            _ = cancel.cancelled() => bail!("Cancelled"),
            chunk = stream.next() => chunk,
        } {
            for data in decoder.push(&chunk.context("OpenAI stream interrupted")?)? {
                if data == "[DONE]" {
                    if chat {
                        self.emit_usage(peer, session_id, agent_id, chat_usage.as_ref())
                            .await?;
                        return chat_stream.finish(text);
                    }
                    continue;
                }
                let event: Value =
                    serde_json::from_str(&data).context("Invalid OpenAI SSE JSON")?;
                if chat {
                    if event["usage"].is_object() {
                        chat_usage = Some(event["usage"].clone());
                    }
                    ensure!(
                        event.get("error").is_none(),
                        "Provider stream reported an error"
                    );
                    if let Some(delta) = chat_stream.push(&event)? {
                        text.push_str(&delta);
                        ensure!(text.len() <= MAX_EVENT, "Model output exceeds stream limit");
                        emit_text(peer, session_id, agent_id, &delta).await?;
                    }
                    continue;
                }
                match event["type"].as_str().unwrap_or_default() {
                    "response.output_text.delta" | "response.refusal.delta" => {
                        if let Some(delta) = event["delta"].as_str() {
                            ensure!(
                                text.len() + delta.len() <= MAX_EVENT,
                                "Model output exceeds stream limit"
                            );
                            text.push_str(delta);
                            emit_text(peer, session_id, agent_id, delta).await?;
                        }
                    }
                    "response.completed" | "response.incomplete" => {
                        let response = &event["response"];
                        self.emit_usage(peer, session_id, agent_id, response.get("usage"))
                            .await?;
                        let output = response["output"]
                            .as_array()
                            .context("Missing response output")?
                            .clone();
                        ensure!(output.len() <= 128, "Too many model output items");
                        return Ok(Response {
                            output,
                            text,
                            incomplete: event["type"] == "response.incomplete",
                        });
                    }
                    "response.failed" | "error" => bail!("OpenAI stream reported an error"),
                    _ => {}
                }
            }
        }
        bail!("OpenAI stream ended before a completed response; no tools were executed")
    }

    async fn emit_usage(
        &self,
        peer: &Peer,
        session: &str,
        agent: Option<&str>,
        usage: Option<&Value>,
    ) -> Result<()> {
        if let Some(usage) = usage.and_then(|usage| self.metrics.record(usage)) {
            peer.send(json!({"jsonrpc":"2.0","method":"_harness/usage","params":{"sessionId":session,"agentId":agent,"usage":usage}})).await?;
        }
        Ok(())
    }
}

async fn emit_text(peer: &Peer, session: &str, agent: Option<&str>, delta: &str) -> Result<()> {
    if let Some(id) = agent {
        peer.task_event(session, id, json!({"textDelta":delta}))
            .await
    } else {
        peer.update(
            session,
            json!({"sessionUpdate":"agent_message_chunk", "content":{"type":"text", "text":delta}}),
        )
        .await
    }
}

pub fn chat_request(
    model: &str,
    instructions: &str,
    input: &[Value],
    tools: &[Value],
    max_tokens: u32,
) -> Result<Value> {
    let mut messages = vec![json!({"role":"system", "content":instructions})];
    for item in input {
        match item["type"].as_str() {
            Some("function_call") => {
                let call = json!({"id":item["call_id"], "type":"function", "function":{"name":item["name"], "arguments":item["arguments"]}});
                if let Some(last) = messages
                    .last_mut()
                    .filter(|m| m["role"] == "assistant" && m["tool_calls"].is_array())
                {
                    last["tool_calls"].as_array_mut().expect("array").push(call);
                } else {
                    messages.push(json!({"role":"assistant", "content":null, "tool_calls":[call]}));
                }
            }
            Some("function_call_output") => messages.push(
                json!({"role":"tool", "tool_call_id":item["call_id"], "content":item["output"]}),
            ),
            Some("reasoning") | Some("compaction") => {}
            _ if item["role"].is_string() => {
                let content = if let Some(text) = item["content"].as_str() {
                    text.to_owned()
                } else {
                    item["content"]
                        .as_array()
                        .context("Invalid message content")?
                        .iter()
                        .filter_map(|c| c["text"].as_str().or_else(|| c["refusal"].as_str()))
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                messages.push(json!({"role":item["role"], "content":content}));
            }
            _ => bail!("Unsupported conversation item for Chat Completions"),
        }
    }
    let tools: Vec<_> = tools.iter().map(|tool| json!({"type":"function", "function":{"name":tool["name"], "description":tool["description"], "parameters":tool["parameters"]}})).collect();
    Ok(
        json!({"model":model,"messages":messages,"tools":tools,"stream":true,"stream_options":{"include_usage":true},"max_tokens":max_tokens}),
    )
}

#[derive(Default)]
pub struct ChatStream {
    calls: std::collections::BTreeMap<u64, Value>,
    finish: Option<String>,
}

impl ChatStream {
    pub fn push(&mut self, event: &Value) -> Result<Option<String>> {
        let Some(choice) = event["choices"].as_array().and_then(|c| c.first()) else {
            return Ok(None);
        };
        if let Some(reason) = choice["finish_reason"].as_str() {
            self.finish = Some(reason.into());
        }
        if let Some(calls) = choice["delta"]["tool_calls"].as_array() {
            for delta in calls {
                let index = delta["index"]
                    .as_u64()
                    .context("Missing tool delta index")?;
                ensure!(index < 128, "Too many streamed tool calls");
                let call = self.calls.entry(index).or_insert_with(
                    || json!({"type":"function_call", "call_id":"", "name":"", "arguments":""}),
                );
                for (target, part) in [
                    ("call_id", &delta["id"]),
                    ("name", &delta["function"]["name"]),
                    ("arguments", &delta["function"]["arguments"]),
                ] {
                    if let Some(part) = part.as_str() {
                        let current = call[target].as_str().unwrap_or_default();
                        ensure!(
                            current.len() + part.len() <= 256 * 1024,
                            "Tool arguments exceed size limit"
                        );
                        call[target] = json!(format!("{current}{part}"));
                    }
                }
            }
        }
        Ok(choice["delta"]["content"].as_str().map(str::to_owned))
    }

    pub fn finish(self, text: String) -> Result<Response> {
        let reason = self
            .finish
            .context("Chat stream ended without a finish reason")?;
        let mut output = Vec::new();
        if !text.is_empty() {
            output.push(json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":text}],"phase":"final_answer"}));
        }
        for call in self.calls.into_values() {
            ensure!(
                call["call_id"].as_str().is_some_and(|s| !s.is_empty())
                    && call["name"].as_str().is_some_and(|s| !s.is_empty()),
                "Incomplete streamed tool call"
            );
            output.push(call);
        }
        Ok(Response {
            output,
            text,
            incomplete: !["stop", "tool_calls"].contains(&reason.as_str()),
        })
    }
}

#[derive(Default)]
pub struct SseDecoder {
    line: Vec<u8>,
    data: String,
}

impl SseDecoder {
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<String>> {
        let mut events = Vec::new();
        for byte in bytes {
            if *byte != b'\n' {
                ensure!(
                    self.line.len() + self.data.len() < MAX_EVENT,
                    "SSE event exceeds size limit"
                );
                self.line.push(*byte);
                continue;
            }
            let line = std::str::from_utf8(&self.line)
                .context("Invalid SSE UTF-8")?
                .trim_end_matches('\r');
            if line.is_empty() {
                if !self.data.is_empty() {
                    events.push(std::mem::take(&mut self.data));
                }
            } else if let Some(data) = line.strip_prefix("data:") {
                if !self.data.is_empty() {
                    self.data.push('\n');
                }
                self.data.push_str(data.strip_prefix(' ').unwrap_or(data));
            }
            self.line.clear();
        }
        Ok(events)
    }
}
