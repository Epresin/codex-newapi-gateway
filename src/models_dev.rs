//! Public specifications only; this client never receives upstream credentials.
use crate::{
    config::{Metadata, Store, atomic_write},
    proxy::http_client,
};
use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, time::Duration};

pub const ENDPOINT: &str = "https://models.dev/api.json";

pub const DEFAULT_CANONICAL_PROVIDERS: &[&str] = &[
    "openai",
    "anthropic",
    "google",
    "xai",
    "moonshotai",
    "deepseek",
    "mistral",
    "cohere",
    "meta",
    "qwen",
];

#[derive(Clone, Debug, PartialEq)]
pub struct Spec {
    pub context: Option<u64>,
    pub efforts: Vec<String>,
    pub image: bool,
}

#[derive(Clone, Default)]
pub struct Database {
    pub records: BTreeMap<String, Spec>,
    pub warning: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct Cache {
    checked_at: u64,
    etag: Option<String>,
    data: Value,
}

pub fn parse(data: &Value) -> Result<Database> {
    let providers = data
        .as_object()
        .context("models.dev payload is not a JSON object")?;
    let mut records = BTreeMap::new();
    for (provider, row) in providers {
        let Some(models) = row["models"].as_object() else {
            continue;
        };
        for (id, model) in models {
            if !model.is_object() {
                bail!("models.dev model entry is not a JSON object");
            }
            let context = model["limit"]["context"]
                .as_u64()
                .filter(|v| (4096..=32_000_000).contains(v));
            let mut efforts = Vec::new();
            if let Some(options) = model["reasoning_options"].as_array() {
                for option in options.iter().filter(|o| o["type"] == "effort") {
                    if let Some(values) = option["values"].as_array() {
                        for value in values.iter().filter_map(Value::as_str) {
                            if matches!(
                                value,
                                "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
                            ) && !efforts.iter().any(|s| s == value)
                            {
                                efforts.push(value.to_owned());
                            }
                        }
                    }
                }
            }
            let image = model["modalities"]["input"]
                .as_array()
                .is_some_and(|v| v.iter().any(|v| v == "image"));
            records.insert(
                format!("{provider}/{id}"),
                Spec {
                    context,
                    efforts,
                    image,
                },
            );
        }
    }
    if records.is_empty() {
        bail!("models.dev returned no valid model records");
    }
    Ok(Database {
        records,
        warning: None,
    })
}

impl Database {
    pub fn lookup(
        &self,
        id: &str,
        explicit: Option<&str>,
        canonical_providers: &[String],
    ) -> Option<(&str, &Spec)> {
        if let Some(key) = explicit {
            return self
                .records
                .get_key_value(key)
                .map(|(k, v)| (k.as_str(), v));
        }
        if let Some((key, spec)) = self.records.get_key_value(id) {
            return Some((key, spec));
        }
        let matches: Vec<_> = self
            .records
            .iter()
            .filter(|(k, _)| k.split_once('/').is_some_and(|(_, model)| model == id))
            .collect();
        let canonical: Vec<_> = matches
            .iter()
            .filter(|(k, _)| {
                let provider = k.split('/').next().unwrap_or_default();
                canonical_providers.iter().any(|p| p == provider)
            })
            .collect();
        if canonical.len() == 1 {
            return Some((canonical[0].0.as_str(), canonical[0].1));
        }
        if matches.len() == 1
            || (!matches.is_empty() && matches.iter().all(|(_, s)| *s == matches[0].1))
        {
            return Some((matches[0].0.as_str(), matches[0].1));
        }
        None
    }
}

pub fn cached(store: &Store) -> Database {
    read_cache(store)
        .and_then(|c| parse(&c.data).ok())
        .unwrap_or_default()
}

fn read_cache(store: &Store) -> Option<Cache> {
    serde_json::from_slice(&std::fs::read(store.dir.join("models-dev-cache.json")).ok()?).ok()
}

pub async fn fetch(store: &Store, metadata: &Metadata, now: u64, force: bool) -> Result<Database> {
    let old = read_cache(store).filter(|c| parse(&c.data).is_ok());
    if !force
        && let Some(cache) = &old
        && now.saturating_sub(cache.checked_at) < metadata.refresh_interval_secs
    {
        return parse(&cache.data);
    }
    let result = async {
        let timeout = Duration::from_secs(metadata.fetch_timeout_secs);
        let mut request = http_client(timeout, timeout)?
            .get(&metadata.url)
            .header("accept", "application/json")
            .timeout(timeout);
        if let Some(tag) = old.as_ref().and_then(|c| c.etag.as_ref()) {
            request = request.header("if-none-match", tag);
        }
        let response = request
            .send()
            .await
            .context("failed to connect to metadata endpoint")?;
        let cache = if response.status() == reqwest::StatusCode::NOT_MODIFIED {
            let previous = old
                .as_ref()
                .context("metadata endpoint returned 304 without valid cache")?;
            Cache {
                checked_at: now,
                etag: previous.etag.clone(),
                data: previous.data.clone(),
            }
        } else {
            if !response.status().is_success() {
                bail!(
                    "metadata endpoint returned HTTP {}",
                    response.status().as_u16()
                );
            }
            let etag = response
                .headers()
                .get("etag")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            let mut bytes = Vec::new();
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                bytes.extend_from_slice(&chunk?);
                if bytes.len() > metadata.max_response_bytes {
                    bail!(
                        "metadata payload exceeds {} bytes",
                        metadata.max_response_bytes
                    );
                }
            }
            Cache {
                checked_at: now,
                etag,
                data: serde_json::from_slice(&bytes)
                    .context("metadata response is not valid JSON")?,
            }
        };
        let db = parse(&cache.data)?;
        atomic_write(
            &store.dir.join("models-dev-cache.json"),
            &serde_json::to_vec(&cache)?,
        )?;
        Ok::<_, anyhow::Error>(db)
    }
    .await;
    match result {
        Ok(db) => Ok(db),
        Err(error) => {
            let mut db = old
                .as_ref()
                .and_then(|c| parse(&c.data).ok())
                .unwrap_or_default();
            db.warning = Some(format!(
                "metadata refresh failed; using cached/fallback data: {error:#}"
            ));
            Ok(db)
        }
    }
}
