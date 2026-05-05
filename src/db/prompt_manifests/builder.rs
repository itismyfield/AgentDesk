use anyhow::{Result, anyhow};

use super::model::{PromptContentVisibility, PromptManifest, PromptManifestLayer};
use super::redaction::{normalized_opt_owned, usize_to_i64};

pub struct PromptManifestBuilder {
    turn_id: String,
    channel_id: String,
    dispatch_id: Option<String>,
    profile: Option<String>,
    layers: Vec<PromptManifestLayer>,
}

impl PromptManifestBuilder {
    pub fn new(turn_id: impl Into<String>, channel_id: impl Into<String>) -> Self {
        Self {
            turn_id: turn_id.into(),
            channel_id: channel_id.into(),
            dispatch_id: None,
            profile: None,
            layers: Vec::new(),
        }
    }

    pub fn dispatch_id(mut self, dispatch_id: impl Into<String>) -> Self {
        self.dispatch_id = normalized_opt_owned(Some(dispatch_id.into()));
        self
    }

    pub fn profile(mut self, profile: impl Into<String>) -> Self {
        self.profile = normalized_opt_owned(Some(profile.into()));
        self
    }

    pub fn layer(mut self, layer: PromptManifestLayer) -> Self {
        self.layers.push(layer);
        self
    }

    pub fn content_layer(
        self,
        layer_name: impl Into<String>,
        enabled: bool,
        source: Option<impl Into<String>>,
        reason: Option<impl Into<String>>,
        content_visibility: PromptContentVisibility,
        content: impl Into<String>,
    ) -> Self {
        self.layer(PromptManifestLayer::from_content(
            layer_name,
            enabled,
            source,
            reason,
            content_visibility,
            content,
        ))
    }

    pub fn build(self) -> Result<PromptManifest> {
        let turn_id = self.turn_id.trim().to_string();
        if turn_id.is_empty() {
            return Err(anyhow!("prompt manifest turn_id is required"));
        }
        let channel_id = self.channel_id.trim().to_string();
        if channel_id.is_empty() {
            return Err(anyhow!("prompt manifest channel_id is required"));
        }
        let mut manifest = PromptManifest {
            id: None,
            created_at: None,
            turn_id,
            channel_id,
            dispatch_id: self.dispatch_id,
            profile: self.profile,
            total_input_tokens_est: 0,
            layer_count: 0,
            layers: self.layers,
        };
        manifest.recompute_totals();
        Ok(manifest)
    }
}

pub fn estimate_tokens_from_chars(chars: usize) -> i64 {
    usize_to_i64(chars / 4)
}
