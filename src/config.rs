use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub auth: String,
    pub wire_api: String,
    pub api_key_env: String,
    pub model: String,
    pub base_url: String,
    pub reasoning_effort: Option<String>,
    pub prompt_cache_retention: Option<String>,
    pub state_dir: PathBuf,
    pub max_turn_steps: usize,
    pub max_subagent_depth: usize,
    pub max_subagents: usize,
    pub max_model_requests: usize,
    pub max_mcp_connections: usize,
    pub max_searches: usize,
    pub max_jobs: usize,
    pub max_sessions: usize,
    pub context_bytes: usize,
    pub max_output_tokens: u32,
    pub tool_output_bytes: usize,
    pub command_timeout_secs: u64,
    pub request_timeout_secs: u64,
    pub computer_use: bool,
    pub computer_xdotool: String,
    pub computer_import: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            auth: "auto".into(),
            wire_api: "responses".into(),
            api_key_env: "OPENAI_API_KEY".into(),
            model: "gpt-6-luna".into(),
            base_url: "https://api.openai.com/v1".into(),
            reasoning_effort: None,
            prompt_cache_retention: None,
            state_dir: default_state_dir(),
            max_turn_steps: 80,
            max_subagent_depth: 2,
            max_subagents: 256,
            max_model_requests: 256,
            max_mcp_connections: 32,
            max_searches: 8,
            max_jobs: 16,
            max_sessions: 256,
            context_bytes: 384 * 1024,
            max_output_tokens: 16384,
            tool_output_bytes: 32 * 1024,
            command_timeout_secs: 120,
            request_timeout_secs: 300,
            computer_use: false,
            computer_xdotool: "xdotool".into(),
            computer_import: "import".into(),
        }
    }
}

impl Config {
    pub async fn load(path: Option<&Path>) -> Result<Self> {
        // Never load executable settings from the model-controlled working tree.
        let mut config: Self = match path {
            Some(path) => toml::from_str(&tokio::fs::read_to_string(path).await?)
                .context("Invalid harness configuration")?,
            None => Self::default(),
        };
        for (name, target) in [
            ("OPENAI_MODEL", &mut config.model),
            ("OPENAI_BASE_URL", &mut config.base_url),
        ] {
            if let Ok(value) = std::env::var(name) {
                *target = value;
            }
        }
        if let Some(path) = std::env::var_os("CODEX_HARNESS_STATE_DIR") {
            config.state_dir = path.into();
        }
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=256).contains(&self.max_mcp_connections),
            "max_mcp_connections must be 1..=256"
        );
        for program in [&self.computer_xdotool, &self.computer_import] {
            ensure!(
                !program.is_empty() && program.len() <= 4096 && !program.contains('\0'),
                "Invalid computer backend executable"
            );
        }
        ensure!(
            self.state_dir.is_absolute() && self.state_dir.parent().is_some(),
            "state_dir must be absolute and cannot be a filesystem root"
        );
        if let Some(retention) = &self.prompt_cache_retention {
            ensure!(
                ["in-memory", "24h"].contains(&retention.as_str()),
                "prompt_cache_retention must be in-memory or 24h (only for supported OpenAI models)"
            );
        }
        ensure!(
            ["auto", "chatgpt", "api_key", "none"].contains(&self.auth.as_str()),
            "auth must be auto, chatgpt, api_key, or none"
        );
        ensure!(
            ["responses", "chat_completions"].contains(&self.wire_api.as_str()),
            "wire_api must be responses or chat_completions"
        );
        ensure!(!self.api_key_env.is_empty(), "api_key_env cannot be empty");
        let url = reqwest::Url::parse(&self.base_url).context("Invalid OpenAI base URL")?;
        let local = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
        ensure!(
            url.username().is_empty() && url.password().is_none(),
            "Credentials must not be in the base URL"
        );
        ensure!(
            url.query().is_none() && url.fragment().is_none(),
            "Base URL cannot contain query or fragment"
        );
        ensure!(
            url.scheme() == "https" || (local && url.scheme() == "http"),
            "Provider URL requires HTTPS (except loopback)"
        );
        ensure!(!self.model.trim().is_empty(), "Model cannot be empty");
        ensure!(
            self.max_turn_steps > 0 && self.max_turn_steps <= 1000,
            "max_turn_steps must be 1..=1000"
        );
        ensure!(
            self.max_subagent_depth <= 4,
            "max_subagent_depth must be <= 4"
        );
        ensure!(
            (1..=1024).contains(&self.max_subagents),
            "max_subagents must be 1..=1024"
        );
        ensure!(
            (1..=128).contains(&self.max_jobs),
            "max_jobs must be 1..=128"
        );
        ensure!(
            (1..=1024).contains(&self.max_sessions),
            "max_sessions must be 1..=1024"
        );
        ensure!(
            (1..=1024).contains(&self.max_model_requests),
            "max_model_requests must be 1..=1024"
        );
        ensure!(
            (1..=32).contains(&self.max_searches),
            "max_searches must be 1..=32"
        );
        ensure!(
            (16384..=4 * 1024 * 1024).contains(&self.context_bytes),
            "context_bytes must be 16KiB..=4MiB"
        );
        ensure!(
            (1024..=256 * 1024).contains(&self.tool_output_bytes),
            "tool_output_bytes must be 1KiB..=256KiB"
        );
        ensure!(
            (256..=65536).contains(&self.max_output_tokens),
            "max_output_tokens must be 256..=65536"
        );
        ensure!(
            (1..=3600).contains(&self.command_timeout_secs),
            "command_timeout_secs must be 1..=3600"
        );
        ensure!(
            (1..=3600).contains(&self.request_timeout_secs),
            "request_timeout_secs must be 1..=3600"
        );
        if let Some(effort) = &self.reasoning_effort
            && !["none", "minimal", "low", "medium", "high", "xhigh"].contains(&effort.as_str())
        {
            bail!("Unsupported reasoning_effort");
        }
        Ok(())
    }
}

// Keep the established XDG layout on Unix; Windows has no HOME by default.
fn default_state_dir() -> PathBuf {
    #[cfg(windows)]
    let state = std::env::var_os("LOCALAPPDATA")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .filter(|path| !path.is_empty())
                .map(|home| PathBuf::from(home).join("AppData/Local"))
        });
    #[cfg(not(windows))]
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")));
    state
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| PathBuf::from("."))
        .join("codex-harness")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_state_path_is_absolute_and_valid() {
        let config = Config::default();
        assert!(config.state_dir.is_absolute());
        config.validate().unwrap();
    }
}
