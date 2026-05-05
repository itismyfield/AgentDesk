use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::{PromptManifestRetentionConfig, PromptManifestVisibilityKind};

use super::redaction::{
    apply_byte_cap, estimate_tokens_from_chars_i64, normalized_opt_owned, redacted_preview,
    sha256_hex, usize_to_i64,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptContentVisibility {
    AdkProvided,
    UserDerived,
}

impl PromptContentVisibility {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AdkProvided => "adk_provided",
            Self::UserDerived => "user_derived",
        }
    }

    pub fn kind(self) -> PromptManifestVisibilityKind {
        match self {
            Self::AdkProvided => PromptManifestVisibilityKind::AdkProvided,
            Self::UserDerived => PromptManifestVisibilityKind::UserDerived,
        }
    }
}

impl TryFrom<&str> for PromptContentVisibility {
    type Error = anyhow::Error;

    fn try_from(value: &str) -> Result<Self> {
        match value.trim() {
            "adk_provided" => Ok(Self::AdkProvided),
            "user_derived" => Ok(Self::UserDerived),
            other => Err(anyhow!("unknown prompt content visibility: {other}")),
        }
    }
}

impl std::fmt::Display for PromptContentVisibility {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptManifest {
    pub id: Option<i64>,
    pub created_at: Option<DateTime<Utc>>,
    pub turn_id: String,
    pub channel_id: String,
    pub dispatch_id: Option<String>,
    pub profile: Option<String>,
    pub total_input_tokens_est: i64,
    pub layer_count: i64,
    pub layers: Vec<PromptManifestLayer>,
}

impl PromptManifest {
    pub fn recompute_totals(&mut self) {
        self.total_input_tokens_est = self
            .layers
            .iter()
            .fold(0_i64, |sum, layer| sum.saturating_add(layer.tokens_est));
        self.layer_count = usize_to_i64(self.layers.len());
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptManifestLayer {
    pub id: Option<i64>,
    pub manifest_id: Option<i64>,
    pub layer_name: String,
    pub enabled: bool,
    pub source: Option<String>,
    pub reason: Option<String>,
    pub chars: i64,
    pub tokens_est: i64,
    pub content_sha256: String,
    pub content_visibility: PromptContentVisibility,
    pub full_content: Option<String>,
    pub redacted_preview: Option<String>,
    /// True when stored body was clipped at write time by the per-layer cap.
    /// `content_sha256` always reflects the *original* content, never the
    /// truncated body.
    #[serde(default)]
    pub is_truncated: bool,
    /// Byte length of the original content. Lets the dashboard report the true
    /// storage cost of a manifest even after retention trim.
    #[serde(default)]
    pub original_bytes: Option<i64>,
}

impl PromptManifestLayer {
    pub fn from_content(
        layer_name: impl Into<String>,
        enabled: bool,
        source: Option<impl Into<String>>,
        reason: Option<impl Into<String>>,
        content_visibility: PromptContentVisibility,
        content: impl Into<String>,
    ) -> Self {
        Self::from_content_with_retention(
            layer_name,
            enabled,
            source,
            reason,
            content_visibility,
            content,
            None,
        )
    }

    /// Variant of [`Self::from_content`] that consults a retention config to
    /// apply per-layer write-time truncation. The `content_sha256` is always
    /// computed against the *original* content so audit hashes survive trims.
    pub fn from_content_with_retention(
        layer_name: impl Into<String>,
        enabled: bool,
        source: Option<impl Into<String>>,
        reason: Option<impl Into<String>>,
        content_visibility: PromptContentVisibility,
        content: impl Into<String>,
        retention: Option<&PromptManifestRetentionConfig>,
    ) -> Self {
        let content = content.into();
        let chars = usize_to_i64(content.chars().count());
        let tokens_est = estimate_tokens_from_chars_i64(chars);
        let content_sha256 = sha256_hex(&content);
        let original_bytes = Some(usize_to_i64(content.len()));

        let cap = retention
            .filter(|cfg| cfg.enabled)
            .and_then(|cfg| cfg.cap_for(content_visibility.kind()));

        let (full_content, redacted_preview, is_truncated) = match content_visibility {
            PromptContentVisibility::AdkProvided => {
                let (body, truncated) = apply_byte_cap(content, cap);
                (Some(body), None, truncated)
            }
            PromptContentVisibility::UserDerived => {
                let preview = redacted_preview(&content);
                // The redacted preview is already clamped by character count;
                // additionally clamp it by byte cap so a wide-char preview can't
                // blow past the user-derived budget.
                let (preview, truncated) = match preview {
                    Some(text) => {
                        let (body, t) = apply_byte_cap(text, cap);
                        (Some(body), t)
                    }
                    None => (None, false),
                };
                (None, preview, truncated)
            }
        };

        Self {
            id: None,
            manifest_id: None,
            layer_name: layer_name.into(),
            enabled,
            source: normalized_opt_owned(source.map(Into::into)),
            reason: normalized_opt_owned(reason.map(Into::into)),
            chars,
            tokens_est,
            content_sha256,
            content_visibility,
            full_content,
            redacted_preview,
            is_truncated,
            original_bytes,
        }
    }
}
