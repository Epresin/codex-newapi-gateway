//! Codex model catalog metadata formatting.
use crate::{config::Catalog, model_policy::Compaction};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeSet;

const INSTRUCTIONS: &str = "You are a coding assistant working in the user's workspace. Follow the user's requests and project instructions. Use available tools to inspect and edit files, verify relevant changes, and explain results accurately. Do not claim actions or tests that you did not perform.";
pub const DEFAULT_REASONING: &str = "xhigh";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceTier {
    pub id: String,
    pub name: String,
    pub description: String,
}

impl ServiceTier {
    pub fn validate(&self) -> Result<()> {
        if !is_ascii_identifier(&self.id) {
            bail!("service tier id must be a non-empty ASCII identifier");
        }
        if self.name.trim().is_empty() {
            bail!("service tier name must not be empty");
        }
        if self.description.trim().is_empty() {
            bail!("service tier description must not be empty");
        }
        Ok(())
    }
}

pub fn default_service_tiers() -> Vec<ServiceTier> {
    vec![ServiceTier {
        id: "priority".into(),
        name: "Fast".into(),
        description: "fast mode".into(),
    }]
}

pub fn default_additional_speed_tiers() -> Vec<String> {
    vec!["fast".into()]
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelMetadata {
    pub compaction: Compaction,
    pub context_window: u64,
    pub reasoning_levels: Vec<String>,
    pub default_reasoning: String,
    pub image_input: bool,
    pub reasoning_summary: bool,
    pub parallel_tools: bool,
    pub instructions: String,
    pub service_tiers: Vec<ServiceTier>,
    pub additional_speed_tiers: Vec<String>,
    pub use_responses_lite: bool,
}

impl Default for ModelMetadata {
    fn default() -> Self {
        Self {
            compaction: Compaction::default(),
            context_window: 1_000_000,
            reasoning_levels: vec!["high".into(), "xhigh".into(), "max".into()],
            default_reasoning: DEFAULT_REASONING.into(),
            image_input: false,
            reasoning_summary: false,
            parallel_tools: false,
            instructions: INSTRUCTIONS.into(),
            service_tiers: default_service_tiers(),
            additional_speed_tiers: default_additional_speed_tiers(),
            use_responses_lite: false,
        }
    }
}

impl ModelMetadata {
    pub fn validate(&self) -> Result<()> {
        self.validate_with_ultra(false)
    }

    pub fn validate_with_ultra(&self, allow_implicit_ultra: bool) -> Result<()> {
        self.compaction.validate()?;
        if !(4096..=32_000_000).contains(&self.context_window) {
            bail!("context_window must be between 4096 and 32000000");
        }
        let mut seen = BTreeSet::new();
        for level in &self.reasoning_levels {
            if !is_ascii_identifier(level) || !seen.insert(level) {
                bail!("reasoning_levels must contain unique ASCII identifiers");
            }
        }
        if self.reasoning_levels.is_empty() {
            if !self.default_reasoning.is_empty()
                && !(allow_implicit_ultra && self.default_reasoning == "ultra")
            {
                bail!("default_reasoning must be empty when reasoning_levels is empty");
            }
        } else if !self.reasoning_levels.contains(&self.default_reasoning)
            && !(allow_implicit_ultra && self.default_reasoning == "ultra")
        {
            bail!("default_reasoning must be included in reasoning_levels");
        }
        if self.instructions.trim().is_empty() {
            bail!("instructions must not be empty");
        }
        let mut tier_ids = BTreeSet::new();
        for tier in &self.service_tiers {
            tier.validate()?;
            if !tier_ids.insert(&tier.id) {
                bail!("duplicate service tier id: {}", tier.id);
            }
        }
        let mut speed_tiers = BTreeSet::new();
        for tier in &self.additional_speed_tiers {
            if !is_ascii_identifier(tier) || !speed_tiers.insert(tier) {
                bail!("additional_speed_tiers must contain unique ASCII identifiers");
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelEntry {
    pub id: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub visible: bool,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub metadata: ModelMetadata,
}

impl ModelEntry {
    pub fn new(id: String, template: &ModelMetadata) -> Self {
        Self {
            enabled: true,
            visible: true,
            source: String::new(),
            display_name: id.clone(),
            description: String::new(),
            id,
            metadata: template.clone(),
        }
    }

    pub fn validate(&self) -> Result<()> {
        validate_id(&self.id)?;
        self.metadata.validate()
    }
}

fn default_true() -> bool {
    true
}

pub(crate) fn is_ascii_identifier(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

pub fn validate_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 512 || id.chars().any(|c| c.is_control() || c.is_whitespace()) {
        bail!("model id must be non-empty, <= 512 bytes, and contain no whitespace");
    }
    Ok(())
}

pub fn validate_entries(entries: &[ModelEntry]) -> Result<()> {
    if entries.len() > 10_000 {
        bail!("model catalog exceeds 10000 entries");
    }
    let mut seen = BTreeSet::new();
    for entry in entries {
        entry.validate()?;
        if !seen.insert(&entry.id) {
            bail!("duplicate model id: {}", entry.id);
        }
    }
    Ok(())
}

fn format_token_count(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{}m", tokens / 1_000_000)
    } else {
        format!("{}k", tokens / 1_000)
    }
}

fn model_description(entry: &ModelEntry, append_context: bool) -> String {
    let description = entry.description.trim();
    if !append_context {
        return description.to_owned();
    }
    let context = format_token_count(entry.metadata.context_window);
    if description.is_empty() {
        context
    } else {
        format!("{description} · {context}")
    }
}

pub fn build_document(entries: &[ModelEntry], catalog: &Catalog) -> Result<Value> {
    validate_entries(entries)?;
    let mut ordered: Vec<_> = entries.iter().filter(|m| m.enabled).collect();
    if let Some(default_model) = catalog.default_model.as_deref() {
        ordered.sort_by_key(|m| m.id != default_model);
    }
    let auto_review_model = catalog
        .auto_review_models
        .iter()
        .find(|candidate| ordered.iter().any(|entry| entry.id == **candidate))
        .map(String::as_str);

    let models: Vec<_> = ordered
        .into_iter()
        .enumerate()
        .map(|(priority, entry)| {
            let m = &entry.metadata;
            let mut levels: Vec<_> = m
                .reasoning_levels
                .iter()
                .filter(|v| *v != "ultra")
                .cloned()
                .collect();
            if levels.is_empty() {
                levels.push("none".into());
            }
            let ultra_base = levels
                .iter()
                .find(|v| *v == "max")
                .unwrap_or_else(|| levels.last().unwrap())
                .clone();
            if catalog.append_ultra_reasoning {
                levels.push("ultra".into());
            }

            let mut model = json!({
                "slug": entry.id,
                "display_name": if entry.display_name.trim().is_empty() { &entry.id } else { &entry.display_name },
                "description": model_description(entry, catalog.append_context_to_description),
                "default_reasoning_level": if m.default_reasoning.is_empty() { None } else { Some(&m.default_reasoning) },
                "supported_reasoning_levels": levels.iter().map(|effort| json!({
                    "effort": effort,
                    "description": if effort == "ultra" {
                        catalog.ultra_reasoning_description.as_str()
                    } else {
                        effort.as_str()
                    }
                })).collect::<Vec<_>>(),
                "multi_agent_reasoning_effort": ultra_base,
                "service_tiers": &m.service_tiers,
                "additional_speed_tiers": &m.additional_speed_tiers,
                "shell_type": &catalog.shell_type,
                "visibility": if entry.visible { "list" } else { "hide" },
                "supported_in_api": true,
                "priority": priority,
                "base_instructions": m.instructions,
                "model_messages": {"instructions_template": m.instructions},
                "supports_reasoning_summaries": m.reasoning_summary,
                "supports_reasoning_summary_parameter": m.reasoning_summary,
                "support_verbosity": false,
                "default_reasoning_summary": "none",
                "supports_parallel_tool_calls": m.parallel_tools,
                "truncation_policy": {"mode": "tokens", "limit": catalog.truncation_limit},
                "context_window": m.context_window,
                "max_context_window": m.context_window,
                "auto_compact_token_limit": m.compaction.tokens(m.context_window),
                "effective_context_window_percent": catalog.effective_context_window_percent,
                "experimental_supported_tools": [],
                "supports_search_tool": false,
                "use_responses_lite": m.use_responses_lite,
                "input_modalities": if m.image_input { vec!["text", "image"] } else { vec!["text"] }
            });
            if let Some(reviewer) = auto_review_model {
                model["auto_review_model_override"] = json!(reviewer);
            }
            model
        })
        .collect();
    Ok(json!({"models": models}))
}

pub fn parse_ids(doc: &Value) -> Result<Vec<String>> {
    let rows = doc
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("New API model list must contain data[]"))?;
    if rows.len() > 10_000 {
        bail!("model list too large");
    }
    let mut ids = BTreeSet::new();
    for row in rows {
        let id = row
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("model entry is missing id"))?;
        validate_id(id)?;
        ids.insert(id.to_owned());
    }
    Ok(ids.into_iter().collect())
}
