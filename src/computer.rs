use crate::{agent::AgentContext, config::Config};
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
#[cfg(target_os = "linux")]
use std::process::Stdio;
use std::{sync::Arc, time::Duration};
use tokio::sync::Mutex;
#[cfg(target_os = "linux")]
use tokio::{io::AsyncReadExt, process::Command};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;
#[cfg(target_os = "macos")]
use macos as native;
use tokio_util::sync::CancellationToken;
#[cfg(windows)]
use windows as native;

pub const SKILL: &str = include_str!("../skills/computer-use/SKILL.md");
pub const IMAGE_LIMIT: usize = 96 * 1024;

#[derive(Default)]
pub struct Desktop {
    gate: Arc<Mutex<Frame>>,
}

pub fn definition() -> Value {
    json!({"type":"function","name":"computer_use","description":"Operate the primary macOS, Windows or Linux X11 desktop. Take a screenshot before native pointer actions. Read the bundled computer-use skill in your instructions. All actions, including screenshots, require approval. Coordinates refer to the returned screenshot, not the native display. Not sandboxed.","strict":false,
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
    ensure!(supported(), "Computer use is unsupported on this platform");
    validate(args)?;
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
    let owner = format!(
        "{}:{}",
        ctx.session_id,
        ctx.agent_id.as_deref().unwrap_or("root")
    );
    ctx.desktop
        .run_for(&ctx.config, args, &ctx.cancel, &owner)
        .await
}

pub fn supported() -> bool {
    cfg!(any(target_os = "macos", target_os = "linux", windows))
}

#[derive(Default)]
struct Frame {
    #[cfg(any(target_os = "macos", windows))]
    last: Option<Snapshot>,
}
#[cfg(any(target_os = "macos", windows))]
struct Snapshot {
    native: (u32, u32),
    image: (u32, u32),
    owner: String,
}

fn validate(args: &Value) -> Result<()> {
    ensure!(args.is_object(), "Computer arguments must be an object");
    let action = args["action"].as_str().context("Missing computer action")?;
    match action {
        "screenshot" => {}
        "move" | "click" | "double_click" => {
            for coordinate in ["x", "y"] {
                ensure!(
                    args[coordinate].as_u64().is_some_and(|n| n < 32768),
                    "Invalid {coordinate} coordinate"
                );
            }
            if let Some(button) = args.get("button") {
                ensure!(
                    matches!(button.as_str(), Some("left" | "right" | "middle")),
                    "Invalid mouse button"
                );
            }
        }
        "type" => {
            let text = args["text"].as_str().context("Missing text")?;
            ensure!(
                text.len() <= 4096 && !text.contains('\0'),
                "Text exceeds 4096 bytes or contains NUL"
            );
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
        }
        "scroll" => {
            ensure!(
                matches!(
                    args["direction"].as_str(),
                    Some("up" | "down" | "left" | "right")
                ),
                "Invalid scroll direction"
            );
            let amount = args
                .get("amount")
                .map_or(Some(3), Value::as_u64)
                .context("Invalid scroll amount")?;
            ensure!((1..=20).contains(&amount), "Scroll amount must be 1..=20");
        }
        _ => bail!("Unknown computer action"),
    }
    Ok(())
}

impl Desktop {
    pub async fn run(
        &self,
        config: &Config,
        args: &Value,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        self.run_for(config, args, cancel, "direct").await
    }

    async fn run_for(
        &self,
        config: &Config,
        args: &Value,
        cancel: &CancellationToken,
        owner: &str,
    ) -> Result<Value> {
        ensure!(
            config.computer_use && supported(),
            "Computer use is disabled or unsupported"
        );
        validate(args)?;
        let work = cancel.child_token();
        let _cancel_on_drop = work.clone().drop_guard();
        let operation = async {
            let guard = tokio::select! {
                biased;
                _ = work.cancelled() => bail!("Computer action cancelled"),
                guard = self.gate.clone().lock_owned() => guard,
            };
            #[cfg(any(target_os = "macos", windows))]
            {
                let args = args.clone();
                let owner = owner.to_owned();
                let token = work.clone();
                // The worker owns the gate until the OS call finishes, even on timeout.
                tokio::task::spawn_blocking(move || run_native(guard, &args, &token, &owner))
                    .await
                    .context("Native desktop worker failed")?
            }
            #[cfg(target_os = "linux")]
            {
                let _guard = guard;
                let _ = owner;
                self.run_linux(config, args, &work).await
            }
            #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
            {
                let _ = (guard, owner);
                bail!("Unsupported desktop");
            }
        };
        tokio::select! {
            biased;
            _ = cancel.cancelled() => bail!("Computer action cancelled"),
            result = tokio::time::timeout(Duration::from_secs(10), operation) => result.context("Computer action timed out")?,
        }
    }
}

#[cfg(any(target_os = "macos", windows))]
fn run_native(
    mut frame: tokio::sync::OwnedMutexGuard<Frame>,
    args: &Value,
    cancel: &CancellationToken,
    owner: &str,
) -> Result<Value> {
    ensure!(!cancel.is_cancelled(), "Computer action cancelled");
    let action = args["action"].as_str().context("Missing computer action")?;
    match action {
        "screenshot" => {
            frame.last = None;
            let geometry = native::geometry()?;
            ensure!(
                (1..=32768).contains(&geometry.0) && (1..=32768).contains(&geometry.1),
                "Invalid display geometry"
            );
            let (width, height) = viewport(geometry.0, geometry.1);
            let bytes = native::screenshot(width, height, IMAGE_LIMIT)?;
            ensure!(!cancel.is_cancelled(), "Computer action cancelled");
            let image = crate::multimodal::image_dimensions("image/jpeg", &bytes)
                .context("Screenshot is not a supported JPEG")?;
            ensure!(
                bytes.len() <= IMAGE_LIMIT
                    && bytes.ends_with(&[0xff, 0xd9])
                    && image.0 > 0
                    && image.1 > 0
                    && image.0 <= width
                    && image.1 <= height,
                "Invalid screenshot size"
            );
            ensure!(
                native::geometry()? == geometry,
                "Display changed; take a fresh screenshot"
            );
            frame.last = Some(Snapshot {
                native: geometry,
                image,
                owner: owner.to_owned(),
            });
            Ok(
                json!({"structuredContent":{"width":image.0,"height":image.1,"native_width":geometry.0,"native_height":geometry.1,"display":"primary"},"content":[{"type":"text","text":format!("Primary display screenshot {}x{}. Use these image coordinates for pointer actions. Native geometry {}x{}. Desktop access is not workspace scoped.",image.0,image.1,geometry.0,geometry.1)},{"type":"image","mimeType":"image/jpeg","data":STANDARD.encode(bytes)}]}),
            )
        }
        "move" | "click" | "double_click" => {
            let snapshot = frame
                .last
                .as_ref()
                .filter(|s| s.owner == owner)
                .context("Take a fresh screenshot before pointer actions")?;
            ensure!(
                native::geometry()? == snapshot.native,
                "Display changed; take a fresh screenshot"
            );
            let x = args["x"].as_u64().context("Missing x coordinate")?;
            let y = args["y"].as_u64().context("Missing y coordinate")?;
            ensure!(
                x < u64::from(snapshot.image.0) && y < u64::from(snapshot.image.1),
                "Coordinate outside screenshot bounds"
            );
            ensure!(!cancel.is_cancelled(), "Computer action cancelled");
            native::pointer(
                action,
                (x * u64::from(snapshot.native.0) / u64::from(snapshot.image.0)) as u32,
                (y * u64::from(snapshot.native.1) / u64::from(snapshot.image.1)) as u32,
                args["button"].as_str().unwrap_or("left"),
            )?;
            Ok(dispatched(action))
        }
        "type" => {
            native::type_text(args["text"].as_str().context("Missing text")?, cancel)?;
            Ok(dispatched(action))
        }
        "key" => {
            native::key(args["key"].as_str().context("Missing key")?)?;
            Ok(dispatched(action))
        }
        "scroll" => {
            native::scroll(
                args["direction"].as_str().context("Missing direction")?,
                args.get("amount").and_then(Value::as_u64).unwrap_or(3) as u32,
            )?;
            Ok(dispatched(action))
        }
        _ => bail!("Unknown computer action"),
    }
}

#[cfg(any(target_os = "macos", windows))]
fn dispatched(action: &str) -> Value {
    json!({"action":action,"ok":true,"note":"Action dispatched. Take a fresh screenshot to verify the resulting state."})
}

impl Desktop {
    #[cfg(target_os = "linux")]
    async fn run_linux(
        &self,
        config: &Config,
        args: &Value,
        cancel: &CancellationToken,
    ) -> Result<Value> {
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
                        json!({"structuredContent":{"width":width,"height":height,"native_width":geometry[0],"native_height":geometry[1],"display":"primary"},"content":[{"type":"text","text":format!("Screenshot {width}x{height}. Use these screenshot coordinates for pointer actions. Native display: {}x{}. Desktop access is not workspace scoped.",geometry[0],geometry[1])},{"type":"image","mimeType":"image/jpeg","data":data}]}),
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

#[cfg(target_os = "linux")]
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
    let _group = crate::jobs::ProcessGroup::new(&mut child)?;
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
