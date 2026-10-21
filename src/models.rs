//! Small, lazy provider catalogs. No model request is made until a client asks for models.
use crate::{
    auth::{Auth, Credentials},
    config::Config,
};
use anyhow::{Context, Result, ensure};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{HashSet, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
    path::Path,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::Mutex,
    time::{Instant, timeout_at},
};

const MAX_BYTES: usize = 2 * 1024 * 1024;
// ACP advertises the catalog through legacy and modern selectors. Bound the
// normalized metadata before those representations can multiply its size.
const MAX_NORMALIZED_BYTES: usize = 256 * 1024;
const MAX_MODELS: usize = 512;
const TTL: u64 = 300;
// Catalog protocol compatibility, verified against installed official codex-cli 0.163.0.
// Upstream: codex-rs/codex-api/src/endpoint/models.rs and protocol/src/openai_models.rs.
const CODEX_CATALOG_VERSION: &str = "0.163.0";

pub type ModelCatalog = Arc<Vec<ModelInfo>>;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReasoningOption {
    pub effort: String,
    pub description: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub reasoning_efforts: Vec<ReasoningOption>,
    #[serde(default)]
    pub reasoning_capabilities_known: bool,
    pub default_reasoning_effort: Option<String>,
}

impl ModelInfo {
    pub fn configured(id: &str) -> Self {
        Self {
            id: id.into(),
            name: id.into(),
            description: None,
            reasoning_efforts: Vec::new(),
            reasoning_capabilities_known: false,
            default_reasoning_effort: None,
        }
    }
}

#[derive(Default)]
pub(crate) struct Catalog {
    // Held through refresh: parallel sessions share one fetch and the same Arc.
    cached: Mutex<Option<Cached>>,
}

#[derive(Serialize, Deserialize)]
struct Cached {
    identity: String,
    fetched_at: u64,
    models: ModelCatalog,
}

impl Catalog {
    pub async fn invalidate(&self, state_dir: &Path) {
        *self.cached.lock().await = None;
        let _ = tokio::fs::remove_file(state_dir.join("models.json")).await;
    }

    pub async fn get(
        &self,
        client: &reqwest::Client,
        auth: &Auth,
        config: &Config,
    ) -> ModelCatalog {
        let fallback = || Arc::new(vec![ModelInfo::configured(&config.model)]);
        let deadline = Instant::now() + Duration::from_secs(5);
        let Ok(mut cached) = timeout_at(deadline, self.cached.lock()).await else {
            return fallback();
        };
        let Ok(Ok(credentials)) = timeout_at(deadline, auth.credentials(client, false)).await
        else {
            return fallback();
        };
        let identity = identity(&credentials, config);
        let now = now();
        if let Some(entry) = cached.as_ref().filter(|entry| fresh(entry, &identity, now)) {
            return entry.models.clone();
        }
        let path = config.state_dir.join("models.json");
        if let Ok(Ok(entry)) = timeout_at(deadline, read_cache(&path, &identity, now)).await {
            let models = entry.models.clone();
            *cached = Some(entry);
            return models;
        }
        let models = match timeout_at(
            deadline,
            fetch(client, &credentials, &config.model, &config.wire_api),
        )
        .await
        {
            Ok(Ok(models)) => {
                let entry = Cached {
                    identity: identity.clone(),
                    fetched_at: now,
                    models: models.clone(),
                };
                // Persistence is optional and shares the same total deadline.
                let _ = timeout_at(deadline, save_cache(&config.state_dir, &entry)).await;
                models
            }
            _ => fallback(),
        };
        // Negative results also have a short bounded lifetime, avoiding request storms.
        *cached = Some(Cached {
            identity,
            fetched_at: now,
            models: models.clone(),
        });
        models
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn fresh(entry: &Cached, identity: &str, now: u64) -> bool {
    entry.identity == identity && entry.fetched_at <= now && now - entry.fetched_at < TTL
}

fn identity(credentials: &Credentials, config: &Config) -> String {
    // A cache partition fingerprint, not a password hash or an authentication mechanism.
    // High-entropy API keys are never persisted. ChatGPT partitions survive token refresh.
    let mut hasher = DefaultHasher::new();
    credentials.base_url.hash(&mut hasher);
    credentials.account_id.hash(&mut hasher);
    if credentials.account_id.is_none() {
        credentials.bearer.hash(&mut hasher);
    }
    config.wire_api.hash(&mut hasher);
    config.model.hash(&mut hasher);
    // Version the partition when capability semantics change, refreshing older disk caches.
    format!("v2-{:016x}", hasher.finish())
}

async fn fetch(
    client: &reqwest::Client,
    credentials: &Credentials,
    configured: &str,
    wire_api: &str,
) -> Result<ModelCatalog> {
    let mut request = client
        .get(format!(
            "{}/models",
            credentials.base_url.trim_end_matches('/')
        ))
        .header("Accept", "application/json")
        .timeout(Duration::from_secs(5));
    if let Some(token) = &credentials.bearer {
        request = request.bearer_auth(token);
    }
    if let Some(account) = &credentials.account_id {
        request = request
            .query(&[("client_version", CODEX_CATALOG_VERSION)])
            .header("ChatGPT-Account-Id", account)
            .header("originator", "codex_cli_rs");
    }
    let response = request.send().await?;
    ensure!(response.status().is_success(), "Model catalog unavailable");
    ensure!(
        response
            .content_length()
            .is_none_or(|size| size <= MAX_BYTES as u64),
        "Model catalog exceeds size limit"
    );
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        ensure!(
            bytes.len() + chunk.len() <= MAX_BYTES,
            "Model catalog exceeds size limit"
        );
        bytes.extend_from_slice(&chunk);
    }
    let mut models = parse(&bytes, configured)?;
    documented_api_efforts(&mut models, &credentials.base_url, wire_api);
    validate_models(&models)?;
    Ok(Arc::new(models))
}

fn documented_api_efforts(models: &mut [ModelInfo], base_url: &str, wire_api: &str) {
    if base_url.trim_end_matches('/') != "https://api.openai.com/v1" || wire_api != "responses" {
        return;
    }
    // Exact API aliases only; do not infer capabilities from custom names or future families.
    // Verified 2026-10-21 against https://developers.openai.com/api/docs/models/<alias>.
    // Live provider metadata always wins. API /models itself advertises no capabilities.
    for model in models
        .iter_mut()
        .filter(|model| !model.reasoning_capabilities_known && model.reasoning_efforts.is_empty())
    {
        let (efforts, default): (&[&str], Option<&str>) = match model.id.as_str() {
            "gpt-6-astra" => (&["low", "medium", "high", "xhigh", "max"], None),
            "gpt-6.1-sol" => (&["low", "medium", "high", "xhigh", "max"], Some("medium")),
            "gpt-6-sol" | "gpt-6-luna" | "gpt-5.6" | "gpt-5.6-sol" | "gpt-5.6-terra"
            | "gpt-5.6-luna" => (
                &["none", "low", "medium", "high", "xhigh", "max"],
                Some("medium"),
            ),
            "gpt-5.3-codex" => (&["low", "medium", "high", "xhigh"], None),
            _ => continue,
        };
        model.reasoning_efforts = efforts
            .iter()
            .map(|effort| ReasoningOption {
                effort: (*effort).into(),
                description: format!("{effort} reasoning effort"),
            })
            .collect();
        model.reasoning_capabilities_known = true;
        model.default_reasoning_effort = default.map(str::to_owned);
    }
}

fn string<'a>(value: &'a Value, fields: &[&str], limit: usize) -> Option<&'a str> {
    fields
        .iter()
        .filter_map(|field| value[*field].as_str())
        .find(|text| !text.trim().is_empty() && text.len() <= limit)
}

fn identifier(text: &str) -> bool {
    !text.is_empty() && text.len() <= 256 && !text.chars().any(char::is_control)
}

fn effort_identifier(effort: &str) -> bool {
    identifier(effort)
        && effort.len() <= 64
        && effort
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn agent_model(item: &Value, id: &str) -> bool {
    let id = id.to_ascii_lowercase();
    if item["visibility"] == "hide"
        || item["visibility"] == "none"
        || item["show_in_picker"] == false
    {
        return false;
    }
    if item["input_modalities"]
        .as_array()
        .is_some_and(|modalities| !modalities.iter().any(|m| m == "text"))
    {
        return false;
    }
    ![
        "embedding",
        "whisper",
        "tts-",
        "dall-e",
        "gpt-image",
        "chatgpt-image",
        "realtime",
        "audio",
        "transcri",
        "moderation",
        "sora",
    ]
    .iter()
    .any(|part| id.contains(part))
}

fn parse(bytes: &[u8], configured: &str) -> Result<Vec<ModelInfo>> {
    ensure!(bytes.len() <= MAX_BYTES, "Model catalog exceeds size limit");
    let value: Value = serde_json::from_slice(bytes)?;
    let rows = value["models"]
        .as_array()
        .or_else(|| value["data"].as_array())
        .context("Missing model catalog")?;
    ensure!(rows.len() <= MAX_MODELS, "Too many catalog models");
    let mut seen = HashSet::new();
    let mut models = Vec::new();
    for item in rows {
        let Some(id) = string(item, &["slug", "id"], 256).filter(|id| identifier(id)) else {
            continue;
        };
        if (id != configured && !agent_model(item, id)) || !seen.insert(id.to_owned()) {
            continue;
        }
        let options = item["supported_reasoning_levels"]
            .as_array()
            .or_else(|| item["supported_reasoning_efforts"].as_array());
        let mut reasoning_efforts = Vec::new();
        if let Some(options) = options {
            ensure!(options.len() <= 32, "Too many reasoning options");
            for option in options {
                let effort = option.as_str().or_else(|| option["effort"].as_str());
                let Some(effort) = effort.filter(|effort| effort_identifier(effort)) else {
                    continue;
                };
                if reasoning_efforts
                    .iter()
                    .any(|option: &ReasoningOption| option.effort == effort)
                {
                    continue;
                }
                reasoning_efforts.push(ReasoningOption {
                    effort: effort.into(),
                    description: string(option, &["description"], 1024)
                        .unwrap_or(effort)
                        .into(),
                });
            }
        }
        let default = string(
            item,
            &["default_reasoning_level", "default_reasoning_effort"],
            64,
        )
        .filter(|default| {
            reasoning_efforts
                .iter()
                .any(|option| option.effort == *default)
        })
        .map(str::to_owned);
        models.push(ModelInfo {
            id: id.into(),
            name: string(item, &["display_name", "name"], 256)
                .unwrap_or(id)
                .into(),
            description: string(item, &["description"], 4096).map(str::to_owned),
            reasoning_efforts,
            reasoning_capabilities_known: options.is_some(),
            default_reasoning_effort: default,
        });
    }
    if !seen.contains(configured) {
        // Never silently remove an account model to make room for the fallback.
        ensure!(
            models.len() < MAX_MODELS,
            "Too many normalized catalog models"
        );
        models.insert(0, ModelInfo::configured(configured));
    }
    validate_models(&models)?;
    Ok(models)
}

async fn read_cache(path: &Path, identity: &str, now: u64) -> Result<Cached> {
    let mut bytes = Vec::new();
    tokio::fs::File::open(path)
        .await?
        .take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .await?;
    ensure!(bytes.len() <= MAX_BYTES, "Model cache exceeds size limit");
    let entry: Cached = serde_json::from_slice(&bytes)?;
    ensure!(fresh(&entry, identity, now), "Stale model cache");
    validate_models(&entry.models)?;
    Ok(entry)
}

fn validate_models(models: &[ModelInfo]) -> Result<()> {
    // Count serialized bytes without allocating a second potentially large copy.
    struct Budget(usize);
    impl std::io::Write for Budget {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "Normalized model catalog exceeds 256KiB",
                ));
            }
            self.0 -= bytes.len();
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Budget(MAX_NORMALIZED_BYTES), models)?;
    ensure!(
        !models.is_empty() && models.len() <= MAX_MODELS,
        "Invalid cached models"
    );
    let mut seen = HashSet::new();
    for model in models {
        ensure!(
            identifier(&model.id)
                && seen.insert(&model.id)
                && model.name.len() <= 256
                && model.description.as_ref().is_none_or(|d| d.len() <= 4096)
                && model.reasoning_efforts.len() <= 32,
            "Invalid cached model metadata"
        );
        ensure!(
            model
                .reasoning_efforts
                .iter()
                .all(|option| effort_identifier(&option.effort)
                    && option.description.len() <= 1024),
            "Invalid cached reasoning metadata"
        );
        ensure!(
            model
                .default_reasoning_effort
                .as_ref()
                .is_none_or(|default| model
                    .reasoning_efforts
                    .iter()
                    .any(|option| &option.effort == default)),
            "Invalid cached reasoning default"
        );
    }
    Ok(())
}

async fn save_cache(root: &Path, entry: &Cached) -> Result<()> {
    validate_models(&entry.models)?;
    let bytes = serde_json::to_vec(entry)?;
    ensure!(bytes.len() <= MAX_BYTES, "Model cache exceeds size limit");
    tokio::fs::create_dir_all(root).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700)).await?;
    }
    let temporary = root.join(format!(".models-{}.tmp", uuid::Uuid::new_v4()));
    // Clean up on cancellation as well as ordinary write errors.
    struct Temporary(std::path::PathBuf);
    impl Drop for Temporary {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _cleanup = Temporary(temporary.clone());
    async {
        let mut options = tokio::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&temporary).await?;
        file.write_all(&bytes).await?;
        drop(file);
        tokio::fs::rename(&temporary, root.join("models.json")).await?;
        Ok(())
    }
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    fn credentials(token: &str, account: Option<&str>) -> Credentials {
        Credentials {
            base_url: "https://api.openai.com/v1".into(),
            bearer: Some(token.into()),
            account_id: account.map(str::to_owned),
        }
    }

    #[test]
    fn parses_codex_efforts_filters_non_agents_and_retains_configured() {
        let value = json!({"models":[
            {"slug":"coder", "display_name":"Coder", "description":"Codes", "visibility":"list", "default_reasoning_level":"persistent", "supported_reasoning_levels":[{"effort":"low","description":"Fast"},{"effort":"persistent","description":"Long work"}]},
            {"slug":"hidden", "visibility":"hide"},
            {"slug":"gpt-image-1"}, {"slug":"text-embedding-3-small"}, {"slug":"gpt-realtime"},
            {"slug":"audio-only-custom", "input_modalities":["audio"]},
            {"slug":"coder"}
        ]});
        let models = parse(&serde_json::to_vec(&value).unwrap(), "configured-custom").unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "configured-custom");
        assert!(models[0].reasoning_efforts.is_empty());
        assert_eq!(models[1].id, "coder");
        assert_eq!(
            models[1].default_reasoning_effort.as_deref(),
            Some("persistent")
        );
        assert_eq!(models[1].reasoning_efforts[1].description, "Long work");
    }

    #[test]
    fn compatible_models_do_not_invent_reasoning_and_catalog_is_bounded() {
        let value = json!({"data":[{"id":"custom"}, {"id":"reasoner", "supported_reasoning_efforts":["low","high","high"], "default_reasoning_effort":"invalid"}]});
        let models = parse(&serde_json::to_vec(&value).unwrap(), "custom").unwrap();
        assert!(models[0].reasoning_efforts.is_empty());
        assert_eq!(models[1].reasoning_efforts.len(), 2);
        assert_eq!(models[1].default_reasoning_effort, None);
        assert!(parse(&vec![b' '; MAX_BYTES + 1], "x").is_err());
        assert!(
            parse(
                &serde_json::to_vec(&json!({"data":vec![json!({"id":"x"}); MAX_MODELS + 1]}))
                    .unwrap(),
                "x"
            )
            .is_err()
        );
        let value = json!({"data":(0..MAX_MODELS).map(|i|json!({"id":format!("model-{i}")})).collect::<Vec<_>>()});
        let bytes = serde_json::to_vec(&value).unwrap();
        let models = parse(&bytes, "model-0").unwrap();
        assert_eq!(models.len(), MAX_MODELS);
        assert!(models.iter().any(|model| model.id == "model-511"));
        assert!(serde_json::to_vec(&models).unwrap().len() <= MAX_NORMALIZED_BYTES);
        assert!(parse(&bytes, "configured").is_err());
    }

    fn large_reasoning_catalog(count: usize) -> Value {
        let efforts: Vec<_> = (0..32)
            .map(|index| format!("effort-{index:02}-{}", "x".repeat(54)))
            .collect();
        assert!(efforts.iter().all(|effort| effort.len() == 64));
        json!({"data": (0..count).map(|index| json!({
            "id": format!("model-{index}"),
            "supported_reasoning_efforts": efforts,
        })).collect::<Vec<_>>()})
    }

    #[test]
    fn compact_wire_catalog_cannot_expand_past_normalized_budget() {
        let bytes = serde_json::to_vec(&large_reasoning_catalog(MAX_MODELS)).unwrap();
        assert!(bytes.len() < MAX_BYTES, "Fixture must fit the wire budget");
        let error = parse(&bytes, "model-0").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Normalized model catalog exceeds 256KiB")
        );
    }

    #[tokio::test]
    async fn disk_cache_cannot_bypass_normalized_catalog_budget() {
        let directory = tempdir().unwrap();
        let efforts: Vec<_> = (0..32)
            .map(|index| ReasoningOption {
                effort: format!("effort-{index:02}-{}", "x".repeat(54)),
                description: "Reasoning description".repeat(3),
            })
            .collect();
        let entry = Cached {
            identity: "partition".into(),
            fetched_at: 1000,
            models: Arc::new(
                (0..128)
                    .map(|index| ModelInfo {
                        reasoning_efforts: efforts.clone(),
                        ..ModelInfo::configured(&format!("model-{index}"))
                    })
                    .collect(),
            ),
        };
        let bytes = serde_json::to_vec(&entry).unwrap();
        assert!(bytes.len() > MAX_NORMALIZED_BYTES && bytes.len() < MAX_BYTES);
        let path = directory.path().join("models.json");
        tokio::fs::write(&path, bytes).await.unwrap();
        let error = read_cache(&path, "partition", 1000).await.err().unwrap();
        assert!(
            error
                .to_string()
                .contains("Normalized model catalog exceeds 256KiB")
        );
        assert!(save_cache(directory.path(), &entry).await.is_err());
    }

    #[test]
    fn documented_api_efforts_are_exact_official_responses_only_and_metadata_wins() {
        let original = vec![
            ModelInfo::configured("gpt-6.1-sol"),
            ModelInfo::configured("gpt-6-sol"),
            ModelInfo::configured("gpt-6.1-sol-future"),
            ModelInfo {
                reasoning_efforts: vec![ReasoningOption {
                    effort: "server-specific".into(),
                    description: "Live metadata".into(),
                }],
                ..ModelInfo::configured("gpt-6-astra")
            },
        ];
        let mut models = original.clone();
        documented_api_efforts(&mut models, "https://custom.example/v1", "responses");
        assert_eq!(models, original);
        documented_api_efforts(&mut models, "https://api.openai.com/v1", "chat_completions");
        assert_eq!(models, original);
        documented_api_efforts(&mut models, "https://api.openai.com/v1", "responses");
        assert!(
            !models[0]
                .reasoning_efforts
                .iter()
                .any(|option| option.effort == "none")
        );
        assert_eq!(
            models[0].default_reasoning_effort.as_deref(),
            Some("medium")
        );
        assert!(
            models[1]
                .reasoning_efforts
                .iter()
                .any(|option| option.effort == "none")
        );
        assert!(models[2].reasoning_efforts.is_empty());
        assert_eq!(models[3], original[3]);
    }

    #[test]
    fn reasoning_metadata_is_bounded_and_invalid_defaults_are_discarded() {
        let value = json!({"models":[{"slug":"x", "default_reasoning_level":"bad", "supported_reasoning_levels":[{"effort":"low"},{"effort":"x".repeat(65)},{"effort":"bad\n"}]}]});
        let models = parse(&serde_json::to_vec(&value).unwrap(), "x").unwrap();
        assert_eq!(models[0].reasoning_efforts.len(), 1);
        assert_eq!(models[0].default_reasoning_effort, None);
        let value = json!({"models":[{"slug":"x", "supported_reasoning_levels":vec![json!({"effort":"low"});33]}]});
        assert!(parse(&serde_json::to_vec(&value).unwrap(), "x").is_err());
    }

    #[test]
    fn explicit_empty_reasoning_is_authoritative_and_blocks_documented_fallback() {
        let bytes = serde_json::to_vec(&json!({"data":[
            {"id":"gpt-6.1-sol", "supported_reasoning_levels":[]},
            {"id":"gpt-6-sol", "supported_reasoning_efforts":[]},
            {"id":"gpt-6-luna"}
        ]}))
        .unwrap();
        let mut models = parse(&bytes, "gpt-6.1-sol").unwrap();
        assert!(models[0].reasoning_capabilities_known);
        assert!(models[1].reasoning_capabilities_known);
        assert!(!models[2].reasoning_capabilities_known);
        documented_api_efforts(&mut models, "https://api.openai.com/v1", "responses");
        assert!(models[0].reasoning_efforts.is_empty());
        assert!(models[1].reasoning_efforts.is_empty());
        assert!(models[2].reasoning_capabilities_known);
        assert!(!models[2].reasoning_efforts.is_empty());
        let legacy: ModelInfo = serde_json::from_value(json!({"id":"custom","name":"Custom","description":null,"reasoning_efforts":[],"default_reasoning_effort":null})).unwrap();
        assert!(!legacy.reasoning_capabilities_known);
    }

    #[test]
    fn cache_identity_separates_credentials_accounts_and_endpoints() {
        let config = Config::default();
        assert_ne!(
            identity(&credentials("key-a", None), &config),
            identity(&credentials("key-b", None), &config)
        );
        assert_ne!(
            identity(&credentials("key-a", Some("account-a")), &config),
            identity(&credentials("key-a", Some("account-b")), &config)
        );
        assert_eq!(
            identity(&credentials("old-token", Some("account-a")), &config),
            identity(&credentials("refreshed-token", Some("account-a")), &config)
        );
        let mut other_endpoint = credentials("key-a", None);
        other_endpoint.base_url = "https://example.com/v1".into();
        assert_ne!(
            identity(&credentials("key-a", None), &config),
            identity(&other_endpoint, &config)
        );
    }

    #[tokio::test]
    async fn disk_cache_is_scoped_private_bounded_and_expires() {
        let dir = tempdir().unwrap();
        let entry = Cached {
            identity: "partition".into(),
            fetched_at: 1000,
            models: Arc::new(vec![ModelInfo::configured("model")]),
        };
        save_cache(dir.path(), &entry).await.unwrap();
        let path = dir.path().join("models.json");
        assert_eq!(
            read_cache(&path, "partition", 1299).await.unwrap().models[0].id,
            "model"
        );
        assert!(read_cache(&path, "other", 1000).await.is_err());
        assert!(read_cache(&path, "partition", 1300).await.is_err());
        assert!(read_cache(&path, "partition", 999).await.is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        tokio::fs::write(&path, vec![b' '; MAX_BYTES + 1])
            .await
            .unwrap();
        assert!(read_cache(&path, "partition", 1000).await.is_err());
    }

    #[tokio::test]
    async fn concurrent_discovery_fetches_once_and_then_reuses_disk_cache() {
        let dir = tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = Config {
            auth: "none".into(),
            base_url: format!("http://{}/v1", listener.local_addr().unwrap()),
            state_dir: dir.path().join("state"),
            model: "fixture".into(),
            ..Config::default()
        };
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 2048];
            let read = socket.read(&mut request).await.unwrap();
            assert!(
                std::str::from_utf8(&request[..read])
                    .unwrap()
                    .starts_with("GET /v1/models HTTP/1.1")
            );
            let body = r#"{"data":[{"id":"fixture"},{"id":"other"}]}"#;
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).as_bytes()).await.unwrap();
        });
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let auth = Auth::new(&config);
        let catalog = Catalog::default();
        let (first, second) = tokio::join!(
            catalog.get(&client, &auth, &config),
            catalog.get(&client, &auth, &config)
        );
        assert_eq!(first.len(), 2);
        assert!(Arc::ptr_eq(&first, &second));
        server.await.unwrap();
        let cold = Catalog::default().get(&client, &auth, &config).await;
        assert_eq!(cold.as_ref(), first.as_ref());
    }

    #[tokio::test]
    async fn unavailable_provider_returns_configured_model() {
        let dir = tempdir().unwrap();
        let config = Config {
            auth: "none".into(),
            base_url: "http://127.0.0.1:1/v1".into(),
            state_dir: dir.path().join("state"),
            model: "offline-custom".into(),
            ..Config::default()
        };
        let models = Catalog::default()
            .get(&reqwest::Client::new(), &Auth::new(&config), &config)
            .await;
        assert_eq!(models.as_ref(), &[ModelInfo::configured("offline-custom")]);
    }
}
