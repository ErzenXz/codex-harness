use crate::config::Config;
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{io::AsyncWriteExt, sync::Mutex};

// Public Codex OAuth client; see THIRD_PARTY_NOTICES.md for the upstream contract.
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const ISSUER: &str = "https://auth.openai.com";
pub const CHATGPT_BASE: &str = "https://chatgpt.com/backend-api/codex";

#[derive(Clone)]
pub struct Auth {
    config: Config,
    refresh: Arc<Mutex<Option<Value>>>,
    selected: Arc<AtomicU8>,
}

pub struct Credentials {
    pub bearer: Option<String>,
    pub account_id: Option<String>,
    pub base_url: String,
}

impl Auth {
    pub fn new(config: &Config) -> Self {
        Self {
            config: config.clone(),
            refresh: Arc::new(Mutex::new(None)),
            selected: Arc::new(AtomicU8::new(match config.auth.as_str() {
                "chatgpt" => 1,
                "api_key" => 2,
                "none" => 3,
                _ => 0,
            })),
        }
    }

    fn path(&self) -> PathBuf {
        self.config.state_dir.join("auth.json")
    }

    pub async fn credentials(&self, client: &reqwest::Client, force: bool) -> Result<Credentials> {
        let mode = match self.selected.load(Ordering::Acquire) {
            1 => "chatgpt",
            2 => "api_key",
            3 => "none",
            _ => "auto",
        };
        self.credentials_for(client, force, mode).await
    }

    pub async fn authenticate(&self, client: &reqwest::Client, method: &str) -> Result<()> {
        self.credentials_for(client, false, method).await?;
        self.selected
            .store(if method == "chatgpt" { 1 } else { 2 }, Ordering::Release);
        Ok(())
    }

    async fn credentials_for(
        &self,
        client: &reqwest::Client,
        force: bool,
        mode: &str,
    ) -> Result<Credentials> {
        let key = std::env::var(&self.config.api_key_env)
            .ok()
            .filter(|k| !k.trim().is_empty());
        let chatgpt = mode == "chatgpt"
            || (mode == "auto"
                && key.is_none()
                && self.config.base_url.trim_end_matches('/') == "https://api.openai.com/v1");
        if !chatgpt {
            ensure!(
                mode == "none" || key.is_some(),
                "Set the configured api_key_env environment variable, or run codex-harness login"
            );
            return Ok(Credentials {
                bearer: if mode == "none" { None } else { key },
                account_id: None,
                base_url: self.config.base_url.clone(),
            });
        }
        ensure!(
            self.config.wire_api == "responses",
            "ChatGPT subscription requires the Responses wire API"
        );
        ensure!(
            self.config.base_url.trim_end_matches('/') == "https://api.openai.com/v1"
                || self.config.base_url.trim_end_matches('/') == CHATGPT_BASE,
            "ChatGPT tokens can only be sent to the OpenAI Codex endpoint"
        );
        let mut cache = self.refresh.lock().await;
        let mut tokens: Value = if let Some(tokens) = cache.as_ref() {
            tokens.clone()
        } else {
            use tokio::io::AsyncReadExt;
            let file = tokio::fs::File::open(self.path())
                .await
                .context("Run codex-harness login to sign in with your ChatGPT subscription")?;
            let mut bytes = Vec::new();
            file.take(65536).read_to_end(&mut bytes).await?;
            ensure!(bytes.len() < 65536, "Invalid credential file size");
            serde_json::from_slice(&bytes).context("Invalid credential file; sign in again")?
        };
        let access = tokens["access_token"]
            .as_str()
            .context("Missing access token; sign in again")?;
        let expiry = claims(access)
            .ok()
            .and_then(|c| c["exp"].as_u64())
            .unwrap_or(0);
        if force || expiry <= now() + 60 {
            let refresh = tokens["refresh_token"]
                .as_str()
                .context("Missing refresh token; sign in again")?;
            let response = client
                .post(format!("{ISSUER}/oauth/token"))
                .json(&json!({
                    "client_id": CLIENT_ID, "grant_type":"refresh_token", "refresh_token":refresh
                }))
                .send()
                .await
                .context("OAuth refresh failed")?;
            ensure!(
                response.status().is_success(),
                "ChatGPT token refresh rejected; run codex-harness login again"
            );
            let updated: Value = response
                .json()
                .await
                .context("Invalid OAuth refresh response")?;
            ensure!(
                updated["access_token"]
                    .as_str()
                    .is_some_and(|token| !token.is_empty()),
                "Missing refreshed access token; sign in again"
            );
            for field in ["access_token", "refresh_token", "id_token"] {
                if let Some(value) = updated[field].as_str() {
                    tokens[field] = json!(value);
                }
            }
            self.save(&tokens).await?;
        }
        *cache = Some(tokens.clone());
        let account_id = tokens["account_id"]
            .as_str()
            .context("Missing ChatGPT account ID; sign in again")?
            .to_owned();
        Ok(Credentials {
            bearer: Some(
                tokens["access_token"]
                    .as_str()
                    .context("Missing refreshed token")?
                    .into(),
            ),
            account_id: Some(account_id),
            base_url: CHATGPT_BASE.into(),
        })
    }

    async fn save(&self, tokens: &Value) -> Result<()> {
        tokio::fs::create_dir_all(&self.config.state_dir).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(
                &self.config.state_dir,
                std::fs::Permissions::from_mode(0o700),
            )
            .await?;
        }
        let path = self
            .config
            .state_dir
            .join(format!(".auth-{}.tmp", uuid::Uuid::new_v4()));
        let mut options = tokio::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        options.mode(0o600);
        let result: Result<()> = async {
            let mut file = options.open(&path).await?;
            file.write_all(&serde_json::to_vec(tokens)?).await?;
            file.sync_all().await?;
            drop(file);
            tokio::fs::rename(&path, self.path()).await?;
            Ok(())
        }
        .await;
        if result.is_err() {
            let _ = tokio::fs::remove_file(&path).await;
        }
        result
    }

    pub async fn login(config: &Config) -> Result<()> {
        let auth = Self::new(config);
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(30))
            .build()?;
        let response = client
            .post(format!("{ISSUER}/api/accounts/deviceauth/usercode"))
            .json(&json!({"client_id":CLIENT_ID}))
            .send()
            .await?;
        ensure!(
            response.status().is_success(),
            "Device sign-in unavailable; enable device-code login in ChatGPT security settings"
        );
        let device: Value = response.json().await?;
        let code = device["user_code"]
            .as_str()
            .or_else(|| device["usercode"].as_str())
            .context("Missing device code")?;
        let id = device["device_auth_id"]
            .as_str()
            .context("Missing device auth ID")?;
        let interval = device["interval"]
            .as_u64()
            .or_else(|| device["interval"].as_str().and_then(|s| s.parse().ok()))
            .unwrap_or(5)
            .clamp(1, 30);
        println!(
            "Open {ISSUER}/codex/device and enter: {code}\nContinue only if you started this login. Expires in 15 minutes."
        );
        let deadline = tokio::time::Instant::now() + Duration::from_secs(900);
        let authorized = loop {
            ensure!(
                tokio::time::Instant::now() < deadline,
                "Device sign-in timed out"
            );
            let response = client
                .post(format!("{ISSUER}/api/accounts/deviceauth/token"))
                .json(&json!({"device_auth_id":id, "user_code":code}))
                .send()
                .await?;
            if response.status().is_success() {
                break response.json::<Value>().await?;
            }
            ensure!(
                [403, 404].contains(&response.status().as_u16()),
                "Device sign-in rejected"
            );
            tokio::time::sleep(Duration::from_secs(interval)).await;
        };
        let response = client
            .post(format!("{ISSUER}/oauth/token"))
            .form(&[
                ("grant_type", "authorization_code"),
                ("client_id", CLIENT_ID),
                (
                    "redirect_uri",
                    "https://auth.openai.com/deviceauth/callback",
                ),
                (
                    "code",
                    authorized["authorization_code"]
                        .as_str()
                        .context("Missing authorization code")?,
                ),
                (
                    "code_verifier",
                    authorized["code_verifier"]
                        .as_str()
                        .context("Missing PKCE verifier")?,
                ),
            ])
            .send()
            .await?;
        ensure!(
            response.status().is_success(),
            "Device token exchange rejected; try signing in again"
        );
        let mut tokens: Value = response.json().await?;
        let id_claims = claims(tokens["id_token"].as_str().context("Missing ID token")?)?;
        let account = id_claims["https://api.openai.com/auth"]["chatgpt_account_id"]
            .as_str()
            .context("No Codex account in ID token; an eligible ChatGPT plan is required")?;
        tokens["account_id"] = json!(account);
        tokens
            .as_object_mut()
            .context("Invalid token response")?
            .retain(|k, _| {
                ["access_token", "refresh_token", "id_token", "account_id"].contains(&k.as_str())
            });
        auth.save(&tokens).await?;
        println!(
            "Signed in. Subscription limits and model availability are set by your ChatGPT plan."
        );
        Ok(())
    }

    pub async fn logout(config: &Config) -> Result<()> {
        match tokio::fs::remove_file(Self::new(config).path()).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

pub fn claims(token: &str) -> Result<Value> {
    let payload = token.split('.').nth(1).context("Invalid OAuth token")?;
    let bytes = URL_SAFE_NO_PAD
        .decode(payload)
        .context("Invalid OAuth token encoding")?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn refreshed_credentials_replace_existing_file_without_temporary_files() {
        let directory = tempfile::tempdir().unwrap();
        let config = Config {
            state_dir: directory.path().join("state"),
            ..Config::default()
        };
        let auth = Auth::new(&config);
        auth.save(&json!({"access_token":"first"})).await.unwrap();
        auth.save(&json!({"access_token":"refreshed"}))
            .await
            .unwrap();
        let saved: Value =
            serde_json::from_slice(&tokio::fs::read(auth.path()).await.unwrap()).unwrap();
        assert_eq!(saved["access_token"], "refreshed");
        assert_eq!(std::fs::read_dir(&config.state_dir).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn failed_credential_save_removes_temporary_file() {
        let directory = tempfile::tempdir().unwrap();
        let config = Config {
            state_dir: directory.path().join("state"),
            ..Config::default()
        };
        let auth = Auth::new(&config);
        tokio::fs::create_dir_all(auth.path()).await.unwrap();
        assert!(auth.save(&json!({"access_token":"test"})).await.is_err());
        assert_eq!(std::fs::read_dir(&config.state_dir).unwrap().count(), 1);
    }
}
