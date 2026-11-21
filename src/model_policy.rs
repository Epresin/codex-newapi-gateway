use crate::{
    catalog::{ModelEntry, ModelMetadata, ServiceTier, is_ascii_identifier},
    config::Config,
    models_dev::Database,
};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum Compaction {
    Percent { percent: f64 },
    Tokens { tokens: u64 },
}

impl Default for Compaction {
    fn default() -> Self {
        Self::Percent { percent: 90.0 }
    }
}

impl Compaction {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Percent { percent }
                if percent.is_finite() && *percent > 0.0 && *percent <= 90.0 =>
            {
                Ok(())
            }
            Self::Tokens { tokens } if *tokens > 0 => Ok(()),
            _ => bail!("compaction percent must be in (0, 90], or tokens must be > 0"),
        }
    }

    pub fn tokens(&self, context: u64) -> u64 {
        match self {
            Self::Percent { percent } => ((context as f64 * percent / 100.0).floor() as u64).max(1),
            Self::Tokens { tokens } => *tokens,
        }
        .min(context * 90 / 100)
    }

    /// Legacy token limits are interpreted once against the effective maximum context.
    pub fn percent(&self, context: u64) -> f64 {
        match self {
            Self::Percent { percent } => *percent,
            Self::Tokens { .. } => self.tokens(context) as f64 * 100.0 / context as f64,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelOverride {
    pub compaction: Option<Compaction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visible: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_levels: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_reasoning: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_input: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_summary: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_tools: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tiers: Option<Vec<ServiceTier>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub extra_service_tiers: Vec<ServiceTier>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub additional_speed_tiers: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub use_responses_lite: Option<bool>,
}

impl ModelOverride {
    pub fn validate(&self, defaults: &ModelMetadata, append_ultra_reasoning: bool) -> Result<()> {
        let mut m = defaults.clone();
        self.apply(&mut m);
        if let Some(effort) = &self.default_reasoning {
            if !is_ascii_identifier(effort) {
                bail!("invalid default reasoning identifier");
            }
            if self.reasoning_levels.is_some()
                && !m.reasoning_levels.contains(effort)
                && !(append_ultra_reasoning && effort == "ultra")
            {
                bail!("default reasoning is not in explicit reasoning levels");
            }
        }
        // Source-provided reasoning levels are unknown until metadata resolution.
        m.default_reasoning = m.reasoning_levels.first().cloned().unwrap_or_default();
        m.validate_with_ultra(append_ultra_reasoning)
    }

    fn apply(&self, m: &mut ModelMetadata) {
        if let Some(v) = &self.compaction {
            m.compaction = v.clone();
        }
        if let Some(v) = self.context_window {
            m.context_window = v;
        }
        if let Some(v) = &self.reasoning_levels {
            m.reasoning_levels = v.clone();
        }
        if let Some(v) = self.image_input {
            m.image_input = v;
        }
        if let Some(v) = self.reasoning_summary {
            m.reasoning_summary = v;
        }
        if let Some(v) = self.parallel_tools {
            m.parallel_tools = v;
        }
        if let Some(v) = &self.instructions {
            m.instructions = v.clone();
        }
        if let Some(v) = &self.service_tiers {
            m.service_tiers = v.clone();
        }
        if !self.extra_service_tiers.is_empty() {
            m.service_tiers.extend(self.extra_service_tiers.clone());
        }
        if let Some(v) = &self.additional_speed_tiers {
            m.additional_speed_tiers = v.clone();
        }
        if let Some(v) = self.use_responses_lite {
            m.use_responses_lite = v;
        }
    }
}

fn normalize_keyword(value: &str) -> String {
    value.to_ascii_lowercase().replace(['-', '_'], "")
}

fn default_model_visibility(id: &str, hidden_keywords: &[String]) -> bool {
    let normalized = normalize_keyword(id);
    !hidden_keywords.iter().any(|keyword| {
        let needle = normalize_keyword(keyword);
        !needle.is_empty() && normalized.contains(&needle)
    })
}

pub fn resolve(ids: &[String], config: &Config, db: &Database) -> Result<Vec<ModelEntry>> {
    ids.iter()
        .map(|id| {
            let p = config.models.get(id).cloned().unwrap_or_default();
            let mut entry = ModelEntry::new(id.clone(), &config.defaults);
            if let Some((key, spec)) = db.lookup(
                id,
                p.source.as_deref(),
                &config.metadata.canonical_providers,
            ) {
                entry.source = format!("models.dev/{key}");
                if let Some(context) = spec.context {
                    entry.metadata.context_window = context;
                }
                entry.metadata.reasoning_levels = spec.efforts.clone();
                entry.metadata.image_input = spec.image;
            } else {
                entry.source = "fallback".into();
            }
            p.apply(&mut entry.metadata);
            entry.metadata.compaction = Compaction::Percent {
                percent: entry
                    .metadata
                    .compaction
                    .percent(entry.metadata.context_window),
            };
            entry.metadata.reasoning_levels.retain(|v| v != "ultra");
            if entry.metadata.reasoning_levels.is_empty() {
                entry.metadata.reasoning_levels.push("none".into());
            }
            if config.catalog.append_ultra_reasoning {
                entry.metadata.reasoning_levels.push("ultra".into());
            }
            let preferred = p
                .default_reasoning
                .as_ref()
                .unwrap_or(&config.defaults.default_reasoning);
            if p.default_reasoning.is_some() && !entry.metadata.reasoning_levels.contains(preferred)
            {
                bail!("invalid default effort for {id}");
            }
            entry.metadata.default_reasoning =
                if entry.metadata.reasoning_levels.contains(preferred) {
                    preferred.clone()
                } else {
                    entry
                        .metadata
                        .reasoning_levels
                        .iter()
                        .rev()
                        .find(|v| v.as_str() != "ultra")
                        .unwrap()
                        .clone()
                };
            entry.enabled = p.enabled.unwrap_or(true);
            entry.visible = p
                .visible
                .unwrap_or_else(|| default_model_visibility(id, &config.catalog.hidden_keywords));
            if let Some(name) = p.display_name {
                entry.display_name = name;
            }
            if let Some(description) = p.description {
                entry.description = description;
            }
            entry.validate()?;
            Ok(entry)
        })
        .collect()
}
