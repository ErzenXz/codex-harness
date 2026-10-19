use crate::{agent::AgentContext, config::Config};
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{process::Stdio, time::Duration};
use tokio::{io::AsyncReadExt, process::Command, sync::Mutex};
use tokio_util::sync::CancellationToken;

pub const SKILL: &str = include_str!("../skills/computer-use/SKILL.md");
pub const IMAGE_LIMIT: usize = 96 * 1024;

#[derive(Default)]
pub struct Desktop {
    gate: Mutex<()>,
}

pub fn definition() -> Value {
    json!({"type":"function","name":"computer_use","description":"Operate the Linux X11 desktop. Read the bundled computer-use skill in your instructions. All actions, including screenshots, require approval. Coordinates refer to the returned screenshot, not the native display. Not sandboxed.","strict":false,
        "parameters":{"type":"object","properties":{
            "action":{"type":"string","enum":["screenshot","move","click","double_click","type","key","scroll"]},
            "x":{"type":"integer","minimum":0},"y":{"type":"integer","minimum":0},
            "text":{"type":"string","maxLength":4096},"key":{"type":"string","maxLength":128},
            "button":{"type":"string","enum":["left","middle","right"]},
            "direction":{"type":"string","enum":["up","down","left","right"]},
            "amount":{"type":"integer","minimum":1,"maximum":20}},"required":["action"],"additionalProperties":false}})
}

pub async fn execute(ctx: &AgentContext, args: &Value, id: &str) -> Result<Value> {
    ensure!(
        ctx.config.computer_use && !ctx.read_only,
        "Computer use is disabled"
    );
    ensure!(
        cfg!(target_os = "linux"),
        "Native computer use requires Linux X11; use an MCP computer server on other platforms"
    );
    ctx.peer
        .permission(
            &ctx.session_id,
            id,
            "computer_use",
            "execute",
            args.clone(),
            &ctx.cancel,
        )
        .await?;
    ctx.desktop.run(&ctx.config, args, &ctx.cancel).await
}

impl Desktop {
    pub async fn run(
        &self,
        config: &Config,
        args: &Value,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        let _guard = tokio::select! {
            _ = cancel.cancelled() => bail!("Computer action cancelled"),
            guard = self.gate.lock() => guard,
        };
        let action = args["action"].as_str().context("Missing computer action")?;
        let mut command: Vec<String> = Vec::new();
        match action {
            "screenshot" | "move" | "click" | "double_click" => {
                let geometry = run(
                    &config.computer_xdotool,
                    &["getdisplaygeometry".into()],
                    256,
                    cancel,
                )
                .await?;
                let geometry = std::str::from_utf8(&geometry)?
                    .split_whitespace()
                    .map(str::parse::<u32>)
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                ensure!(
                    geometry.len() == 2 && geometry.iter().all(|n| (1..=32768).contains(n)),
                    "Invalid display geometry"
                );
                let (width, height) = viewport(geometry[0], geometry[1]);
                if action == "screenshot" {
                    let bytes = run(
                        &config.computer_import,
                        &[
                            "-window".into(),
                            "root".into(),
                            "-resize".into(),
                            format!("{width}x{height}!"),
                            "-strip".into(),
                            "-quality".into(),
                            "45".into(),
                            "jpeg:-".into(),
                        ],
                        IMAGE_LIMIT,
                        cancel,
                    )
                    .await?;
                    let data = STANDARD.encode(&bytes);
                    let dimensions = crate::multimodal::image_dimensions("image/jpeg", &bytes)
                        .context("Screenshot is not a supported JPEG")?;
                    ensure!(
                        dimensions == (width, height),
                        "Unexpected screenshot dimensions"
                    );
                    return Ok(
                        json!({"content":[{"type":"text","text":format!("Screenshot {width}x{height}. Use these screenshot coordinates for pointer actions. Native display: {}x{}. Desktop access is not workspace scoped.",geometry[0],geometry[1])},{"type":"image","mimeType":"image/jpeg","data":data}]}),
                    );
                }
                let x = args["x"].as_u64().context("Missing x coordinate")?;
                let y = args["y"].as_u64().context("Missing y coordinate")?;
                ensure!(
                    x < u64::from(width) && y < u64::from(height),
                    "Coordinate outside screenshot bounds"
                );
                command.extend([
                    "mousemove".into(),
                    (x * u64::from(geometry[0]) / u64::from(width)).to_string(),
                    (y * u64::from(geometry[1]) / u64::from(height)).to_string(),
                ]);
                if action != "move" {
                    let button = match args["button"].as_str().unwrap_or("left") {
                        "left" => "1",
                        "middle" => "2",
                        "right" => "3",
                        _ => bail!("Invalid mouse button"),
                    };
                    command.push("click".into());
                    if action == "double_click" {
                        command.extend([
                            "--repeat".into(),
                            "2".into(),
                            "--delay".into(),
                            "100".into(),
                        ]);
                    }
                    command.push(button.into());
                }
            }
            "type" => {
                let text = args["text"].as_str().context("Missing text")?;
                ensure!(
                    text.len() <= 4096 && !text.contains('\0'),
                    "Text exceeds 4096 bytes or contains NUL"
                );
                command.extend([
                    "type".into(),
                    "--clearmodifiers".into(),
                    "--delay".into(),
                    "0".into(),
                    "--".into(),
                    text.into(),
                ]);
            }
            "key" => {
                let key = args["key"].as_str().context("Missing key")?;
                ensure!(
                    !key.is_empty()
                        && key.len() <= 128
                        && !key.starts_with('-')
                        && key
                            .bytes()
                            .all(|c| c.is_ascii_alphanumeric() || b"_+-".contains(&c)),
                    "Invalid key chord"
                );
                command.extend(["key".into(), "--clearmodifiers".into(), key.into()]);
            }
            "scroll" => {
                let button = match args["direction"].as_str() {
                    Some("up") => "4",
                    Some("down") => "5",
                    Some("left") => "6",
                    Some("right") => "7",
                    _ => bail!("Invalid scroll direction"),
                };
                let amount = args
                    .get("amount")
                    .map_or(Some(3), Value::as_u64)
                    .context("Invalid scroll amount")?;
                ensure!((1..=20).contains(&amount), "Scroll amount must be 1..=20");
                command.extend([
                    "click".into(),
                    "--repeat".into(),
                    amount.to_string(),
                    "--delay".into(),
                    "40".into(),
                    button.into(),
                ]);
            }
            _ => bail!("Unknown computer action"),
        }
        run(&config.computer_xdotool, &command, 1024, cancel).await?;
        Ok(
            json!({"action":action,"ok":true,"note":"Action dispatched. Take a fresh screenshot to verify the resulting state."}),
        )
    }
}

pub fn viewport(width: u32, height: u32) -> (u32, u32) {
    let scale = (1280.0 / f64::from(width))
        .min(960.0 / f64::from(height))
        .min(1.0);
    (
        (f64::from(width) * scale).round().max(1.0) as u32,
        (f64::from(height) * scale).round().max(1.0) as u32,
    )
}

async fn run(
    program: &str,
    args: &[String],
    limit: usize,
    cancel: &CancellationToken,
) -> Result<Vec<u8>> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .env_clear();
    for key in ["PATH", "DISPLAY", "XAUTHORITY", "HOME", "LANG", "LC_ALL"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }
    let mut child=command.spawn().context("Computer backend unavailable; install xdotool and ImageMagick and provide DISPLAY/XAUTHORITY")?;
    let _group = crate::jobs::ProcessGroup(child.id());
    let mut stdout = child
        .stdout
        .take()
        .context("Missing computer output")?
        .take(limit as u64 + 1);
    let operation = async {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).await?;
        ensure!(
            bytes.len() <= limit,
            "Computer output exceeds limit; reduce display complexity/resolution"
        );
        ensure!(
            child.wait().await?.success(),
            "Computer backend failed; check X11 display access"
        );
        Ok(bytes)
    };
    tokio::select! {
        _ = cancel.cancelled() => bail!("Computer action cancelled"),
        result=tokio::time::timeout(Duration::from_secs(10),operation) => result.context("Computer backend timed out")?,
    }
}
