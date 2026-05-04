//! Prompt manifest construction — assembles `PromptManifest` layers and the
//! `RecoveryContextManifestInput` carrier, plus content hashing/preview helpers
//! that feed the persistence hand-off.

use poise::serenity_prelude::ChannelId;
use regex::Regex;
use sha2::{Digest, Sha256};
use std::sync::LazyLock;

use super::DispatchProfile;
use super::dispatch_contract::{CurrentTaskContext, render_dispatch_contract};
use super::memory_guidance::MemoryRecallManifestInput;
use crate::db::prompt_manifests::{
    PromptContentVisibility, PromptManifest, PromptManifestBuilder, PromptManifestLayer,
    estimate_tokens_from_chars,
};
use crate::services::discord::settings::{MemoryBackendKind, ResolvedMemorySettings, RoleBinding};
use crate::services::observability::recovery_audit::{
    RecoveryAuditRecord, recovery_context_sha256,
};

pub(super) const DISPATCH_CONTRACT_LAYER_NAME: &str = "dispatch_contract";
pub(super) const DISPATCH_CONTRACT_LAYER_SOURCE: &str = "prompt_builder.render_dispatch_contract";
pub(super) const CURRENT_TASK_LAYER_NAME: &str = "current_task";
pub(super) const CURRENT_TASK_REDACTED_PREVIEW_MAX_BYTES: usize = 2_000;
pub(super) const RECOVERY_CONTEXT_LAYER_NAME: &str = "recovery_context";
pub(super) const RECOVERY_CONTEXT_LAYER_SOURCE: &str = "Discord recent N messages";
pub(super) const RECOVERY_CONTEXT_LAYER_REASON: &str = "provider-native resume failed";
pub(super) const ROLE_PROMPT_LAYER_NAME: &str = "role_prompt";
pub(super) const MEMORY_RECALL_LAYER_NAME: &str = "memory_recall";
pub(super) const MEMORY_RECALL_LAYER_SOURCE: &str = "memento";

#[derive(Debug, Clone, Copy)]
pub(crate) struct RecoveryContextManifestInput<'a> {
    pub(crate) raw_context: &'a str,
    pub(crate) audit_record: Option<&'a RecoveryAuditRecord>,
}

pub(super) fn current_task_manifest_layer(
    current_task: &CurrentTaskContext<'_>,
    rendered_section: &str,
) -> PromptManifestLayer {
    let dispatch_id = current_task
        .dispatch_id
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let (source, reason) = match dispatch_id {
        Some(dispatch_id) => (
            "task_dispatches.context".to_string(),
            format!("dispatch_id={dispatch_id}"),
        ),
        None => ("discord_message".to_string(), "freeform".to_string()),
    };
    let (chars, tokens_est, content_sha256) = prompt_manifest_content_stats(rendered_section);

    let mut layer = PromptManifestLayer::from_content(
        CURRENT_TASK_LAYER_NAME,
        true,
        Some(source),
        Some(reason),
        PromptContentVisibility::UserDerived,
        rendered_section.to_string(),
    );
    layer.chars = chars;
    layer.tokens_est = tokens_est;
    layer.content_sha256 = content_sha256;
    layer.redacted_preview = Some(redacted_prompt_manifest_preview(rendered_section));
    layer.full_content = None;
    layer
}

pub(super) fn dispatch_contract_manifest_layer(
    dispatch_type: Option<&str>,
    current_task: Option<&CurrentTaskContext<'_>>,
) -> PromptManifestLayer {
    let dispatch_type_reason = dispatch_type
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("none");
    let full_content = current_task.and_then(|task| render_dispatch_contract(dispatch_type, task));
    let (chars, tokens_est, content_sha256) =
        prompt_manifest_content_stats(full_content.as_deref().unwrap_or(""));

    let mut layer = PromptManifestLayer::from_content(
        DISPATCH_CONTRACT_LAYER_NAME,
        full_content.is_some(),
        Some(DISPATCH_CONTRACT_LAYER_SOURCE),
        Some(format!("dispatch_type={dispatch_type_reason}")),
        PromptContentVisibility::AdkProvided,
        full_content.clone().unwrap_or_default(),
    );
    layer.chars = chars;
    layer.tokens_est = tokens_est;
    layer.content_sha256 = content_sha256;
    layer.redacted_preview = None;
    layer.full_content = full_content;
    layer
}

pub(super) fn recovery_context_manifest_layer(
    recovery_context: Option<&RecoveryContextManifestInput<'_>>,
) -> Result<PromptManifestLayer, String> {
    let raw_context = recovery_context
        .map(|context| context.raw_context.trim())
        .filter(|value| !value.is_empty());
    let Some(raw_context) = raw_context else {
        let (chars, tokens_est, content_sha256) = prompt_manifest_content_stats("");
        let mut layer = PromptManifestLayer::from_content(
            RECOVERY_CONTEXT_LAYER_NAME,
            false,
            Some(RECOVERY_CONTEXT_LAYER_SOURCE),
            Some(RECOVERY_CONTEXT_LAYER_REASON),
            PromptContentVisibility::UserDerived,
            "",
        );
        layer.chars = chars;
        layer.tokens_est = tokens_est;
        layer.content_sha256 = content_sha256;
        layer.redacted_preview = None;
        layer.full_content = None;
        return Ok(layer);
    };

    let (chars, tokens_est, _) = prompt_manifest_content_stats(raw_context);
    let content_sha256 = recovery_context_sha256(raw_context);
    if let Some(audit_record) = recovery_context.and_then(|context| context.audit_record)
        && audit_record.content_sha256 != content_sha256
    {
        return Err(format!(
            "recovery_context sha256 mismatch: audit={} prompt={}",
            audit_record.content_sha256, content_sha256
        ));
    }

    let redacted_preview = recovery_context
        .and_then(|context| context.audit_record)
        .map(|record| record.redacted_preview.clone())
        .unwrap_or_else(|| redacted_prompt_manifest_preview(raw_context));

    let mut layer = PromptManifestLayer::from_content(
        RECOVERY_CONTEXT_LAYER_NAME,
        true,
        Some(RECOVERY_CONTEXT_LAYER_SOURCE),
        Some(RECOVERY_CONTEXT_LAYER_REASON),
        PromptContentVisibility::UserDerived,
        raw_context.to_string(),
    );
    layer.chars = chars;
    layer.tokens_est = tokens_est;
    layer.content_sha256 = content_sha256;
    layer.redacted_preview = Some(redacted_preview);
    layer.full_content = None;
    Ok(layer)
}

pub(super) fn prompt_manifest_content_stats(content: &str) -> (i64, i64, String) {
    let char_count = content.chars().count();
    let chars = if char_count > i64::MAX as usize {
        i64::MAX
    } else {
        char_count as i64
    };
    (
        chars,
        estimate_tokens_from_chars(char_count),
        prompt_manifest_content_sha256(content),
    )
}

pub(super) fn prompt_manifest_content_sha256(content: &str) -> String {
    let digest = Sha256::digest(content.as_bytes());
    let mut hex = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut hex, "{byte:02x}");
    }
    hex
}

pub(super) fn redacted_prompt_manifest_preview(input: &str) -> String {
    static EMAIL_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)\b[A-Z0-9._%+-]+@[A-Z0-9.-]+\.[A-Z]{2,}\b").expect("valid email regex")
    });

    let redacted = EMAIL_RE.replace_all(input, "[redacted-email]");
    let redacted =
        crate::services::discord::formatting::redact_sensitive_for_placeholder(&redacted);
    truncate_prompt_manifest_preview(&redacted, CURRENT_TASK_REDACTED_PREVIEW_MAX_BYTES)
}

pub(super) fn truncate_prompt_manifest_preview(input: &str, max_bytes: usize) -> String {
    if input.len() <= max_bytes {
        return input.to_string();
    }

    let mut boundary = max_bytes;
    while boundary > 0 && !input.is_char_boundary(boundary) {
        boundary -= 1;
    }
    let mut truncated = input[..boundary].trim_end().to_string();
    truncated.push_str("\n[... truncated]");
    truncated
}

pub(super) fn role_prompt_manifest_layer(
    binding: &RoleBinding,
    enabled: bool,
    full_content: Option<String>,
) -> PromptManifestLayer {
    let content = full_content.unwrap_or_default();
    let mut layer = PromptManifestLayer::from_content(
        ROLE_PROMPT_LAYER_NAME,
        enabled,
        Some(format!("agents/{}.prompt.md", binding.role_id)),
        Some(format!("agent_id={}", binding.role_id)),
        PromptContentVisibility::AdkProvided,
        content,
    );
    if !enabled {
        layer.full_content = None;
    }
    layer
}

pub(super) fn memory_recall_manifest_layer(
    memory_settings: Option<&ResolvedMemorySettings>,
    memento_mcp_available: bool,
    recall: Option<&MemoryRecallManifestInput<'_>>,
) -> Option<PromptManifestLayer> {
    let memory_settings = memory_settings?;
    let (enabled, reason, content) = if memory_settings.backend != MemoryBackendKind::Memento {
        (
            false,
            format!("memory_backend={}", memory_settings.backend.as_str()),
            "",
        )
    } else if !memento_mcp_available {
        (
            false,
            "memory_backend=memento;mcp_unavailable".to_string(),
            "",
        )
    } else if let Some(recall) = recall {
        let content = recall.external_recall.map(str::trim).unwrap_or_default();
        if recall.should_recall {
            (
                true,
                format!("memory_backend=memento;recall={}", recall.gate_reason),
                content,
            )
        } else {
            (
                false,
                format!(
                    "memory_backend=memento;recall_skipped={}",
                    recall.gate_reason
                ),
                "",
            )
        }
    } else {
        (
            false,
            "memory_backend=memento;recall_state=unknown".to_string(),
            "",
        )
    };

    let (chars, tokens_est, content_sha256) = prompt_manifest_content_stats(content);
    let mut layer = PromptManifestLayer::from_content(
        MEMORY_RECALL_LAYER_NAME,
        enabled,
        Some(MEMORY_RECALL_LAYER_SOURCE),
        Some(reason),
        PromptContentVisibility::UserDerived,
        content.to_string(),
    );
    layer.chars = chars;
    layer.tokens_est = tokens_est;
    layer.content_sha256 = content_sha256;
    layer.redacted_preview =
        (!content.is_empty()).then(|| redacted_prompt_manifest_preview(content));
    layer.full_content = None;
    Some(layer)
}

pub(super) fn prompt_manifest_profile(profile: DispatchProfile) -> &'static str {
    match profile {
        DispatchProfile::Full => "full",
        DispatchProfile::Lite => "lite",
        DispatchProfile::ReviewLite => "review_lite",
    }
}

pub(super) fn build_prompt_manifest(
    turn_id: Option<&str>,
    channel_id: ChannelId,
    profile: DispatchProfile,
    current_task: Option<&CurrentTaskContext<'_>>,
    layers: Vec<PromptManifestLayer>,
) -> Option<PromptManifest> {
    if layers.is_empty() {
        return None;
    }
    let Some(turn_id) = turn_id else {
        return None;
    };

    let mut builder = PromptManifestBuilder::new(turn_id, channel_id.get().to_string())
        .profile(prompt_manifest_profile(profile));
    if let Some(dispatch_id) = current_task.and_then(|task| task.dispatch_id) {
        builder = builder.dispatch_id(dispatch_id);
    }
    for layer in layers {
        builder = builder.layer(layer);
    }

    match builder.build() {
        Ok(manifest) => Some(manifest),
        Err(error) => {
            tracing::warn!("[prompt-manifest] build failed: {error}");
            None
        }
    }
}
