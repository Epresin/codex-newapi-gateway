use crate::{catalog::ModelMetadata, model_policy::ModelOverride};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    net::SocketAddr,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub server: Server,
    pub upstream: Upstream,
    pub auth: Auth,
    pub metadata: Metadata,
    pub catalog: Catalog,
    pub defaults: ModelMetadata,
    pub models: BTreeMap<String, ModelOverride>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Server {
    pub listen: SocketAddr,
    pub reject_browser_origin: bool,
    pub max_inspection_body_bytes: usize,
    pub max_models_response_bytes: usize,
}

impl Default for Server {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:17329".parse().unwrap(),
            reject_browser_origin: true,
            max_inspection_body_bytes: 64 * 1024 * 1024,
            max_models_response_bytes: 8 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Upstream {
    pub base_url: String,
    pub connect_timeout_secs: u64,
    pub read_timeout_secs: u64,
}

impl Default for Upstream {
    fn default() -> Self {
        Self {
            base_url: "http://127.0.0.1:3000/v1".into(),
            connect_timeout_secs: 15,
            read_timeout_secs: 600,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Auth {
    pub key_header: String,
    pub allow_bearer_fallback: bool,
}

impl Default for Auth {
    fn default() -> Self {
        Self {
            key_header: "X-NewAPI-Key".into(),
            allow_bearer_fallback: false,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Metadata {
    pub enabled: bool,
    pub url: String,
    pub refresh_interval_secs: u64,
    pub fetch_timeout_secs: u64,
    pub max_response_bytes: usize,
    pub cache_dir: PathBuf,
    pub canonical_providers: Vec<String>,
}

impl Default for Metadata {
    fn default() -> Self {
        Self {
            enabled: true,
            url: crate::models_dev::ENDPOINT.into(),
            refresh_interval_secs: 21600,
            fetch_timeout_secs: 30,
            max_response_bytes: 32 * 1024 * 1024,
            cache_dir: PathBuf::from("./data/cache"),
            canonical_providers: crate::models_dev::DEFAULT_CANONICAL_PROVIDERS
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Catalog {
    pub default_model: Option<String>,
    pub hidden_keywords: Vec<String>,
    pub auto_review_models: Vec<String>,
    pub append_context_to_description: bool,
    pub append_ultra_reasoning: bool,
    pub ultra_reasoning_description: String,
    pub shell_type: String,
    pub effective_context_window_percent: u8,
    pub truncation_limit: u64,
}

impl Default for Catalog {
    fn default() -> Self {
        Self {
            default_model: None,
            hidden_keywords: vec!["image".into(), "autoreview".into()],
            auto_review_models: Vec::new(),
            append_context_to_description: true,
            append_ultra_reasoning: true,
            ultra_reasoning_description: "Codex proactive delegation mode".into(),
            shell_type: "unified_exec".into(),
            effective_context_window_percent: 95,
            truncation_limit: 10_000,
        }
    }
}

impl Catalog {
    pub fn validate(&self) -> Result<()> {
        if let Some(default_model) = &self.default_model {
            crate::catalog::validate_id(default_model)?;
        }
        for keyword in &self.hidden_keywords {
            if keyword.trim().is_empty() {
                bail!("catalog.hidden_keywords entries must not be empty");
            }
        }
        for model_id in &self.auto_review_models {
            crate::catalog::validate_id(model_id)?;
        }
        if self.append_ultra_reasoning && self.ultra_reasoning_description.trim().is_empty() {
            bail!("catalog.ultra_reasoning_description must not be empty");
        }
        if self.shell_type.trim().is_empty() {
            bail!("catalog.shell_type must not be empty");
        }
        if !(1..=100).contains(&self.effective_context_window_percent) {
            bail!("catalog.effective_context_window_percent must be between 1 and 100");
        }
        if self.truncation_limit == 0 {
            bail!("catalog.truncation_limit must be greater than 0");
        }
        Ok(())
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path)
            .with_context(|| format!("cannot read configuration file: {}", path.display()))?;
        let mut c: Self = toml_edit::de::from_str(&text).context("invalid TOML configuration")?;
        c.upstream.base_url = normalize_url(&c.upstream.base_url)?;
        c.validate()?;
        Ok(c)
    }

    pub fn validate(&self) -> Result<()> {
        if self.server.max_inspection_body_bytes < 1024 {
            bail!("server.max_inspection_body_bytes must be >= 1024");
        }
        if self.server.max_models_response_bytes < 1024 {
            bail!("server.max_models_response_bytes must be >= 1024");
        }
        normalize_url(&self.upstream.base_url)?;
        if self.upstream.connect_timeout_secs == 0 {
            bail!("upstream.connect_timeout_secs must be >= 1");
        }
        if self.upstream.read_timeout_secs == 0 {
            bail!("upstream.read_timeout_secs must be >= 1");
        }
        normalize_url(&self.metadata.url)?;
        if self.metadata.refresh_interval_secs < 60 {
            bail!("metadata.refresh_interval_secs must be >= 60");
        }
        if self.metadata.fetch_timeout_secs == 0 {
            bail!("metadata.fetch_timeout_secs must be >= 1");
        }
        if self.metadata.max_response_bytes < 1024 {
            bail!("metadata.max_response_bytes must be >= 1024");
        }
        for provider in &self.metadata.canonical_providers {
            if provider.trim().is_empty() {
                bail!("metadata.canonical_providers entries must not be empty");
            }
        }
        let name = reqwest::header::HeaderName::from_bytes(self.auth.key_header.as_bytes())
            .context("invalid auth.key_header")?;
        if name == reqwest::header::AUTHORIZATION || name == reqwest::header::COOKIE {
            bail!("dedicated key header cannot be Authorization or Cookie");
        }
        self.catalog.validate()?;
        self.defaults
            .validate_with_ultra(self.catalog.append_ultra_reasoning)?;
        for (id, policy) in &self.models {
            crate::catalog::validate_id(id)?;
            policy.validate(&self.defaults, self.catalog.append_ultra_reasoning)?;
        }
        Ok(())
    }
}

pub fn normalize_url(value: &str) -> Result<String> {
    let u = url::Url::parse(value.trim()).context("invalid upstream/metadata URL")?;
    if !matches!(u.scheme(), "http" | "https")
        || u.host_str().is_none()
        || !u.username().is_empty()
        || u.password().is_some()
        || u.query().is_some()
        || u.fragment().is_some()
    {
        bail!("URL must be HTTP(S) without credentials, query, or fragment");
    }
    Ok(u.as_str().trim_end_matches('/').to_owned())
}

#[derive(Clone)]
pub struct Store {
    pub dir: PathBuf,
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("path has no parent")?;
    fs::create_dir_all(parent)?;
    let mut f = tempfile::NamedTempFile::new_in(parent)?;
    f.write_all(bytes)?;
    f.as_file().sync_all()?;
    f.persist(path).map_err(|e| e.error)?;
    Ok(())
}
