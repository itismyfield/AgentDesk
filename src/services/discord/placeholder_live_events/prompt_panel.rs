use crate::db::prompt_manifests::PromptManifest;

use super::common::{
    PROMPT_PANEL_LINE_MAX_CHARS, PROMPT_PANEL_SKIPPED_REASON_MAX_CHARS,
    escape_status_panel_markdown, truncate_chars,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PromptPanelSnapshot {
    profile: Option<String>,
    enabled_layers: Vec<String>,
    skipped_layers: Vec<SkippedLayerEntry>,
    total_input_tokens_est: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SkippedLayerEntry {
    name: String,
    reason: Option<String>,
}

impl PromptPanelSnapshot {
    pub(super) fn from_manifest(manifest: &PromptManifest) -> Self {
        let mut enabled_layers = Vec::new();
        let mut skipped_layers = Vec::new();
        for layer in &manifest.layers {
            if layer.enabled {
                enabled_layers.push(layer.layer_name.clone());
            } else {
                let reason = layer
                    .reason
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string);
                skipped_layers.push(SkippedLayerEntry {
                    name: layer.layer_name.clone(),
                    reason,
                });
            }
        }
        Self {
            profile: manifest
                .profile
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string),
            enabled_layers,
            skipped_layers,
            total_input_tokens_est: manifest.total_input_tokens_est.max(0),
        }
    }
}

pub(super) fn render_prompt_panel_block(prompt: &PromptPanelSnapshot) -> String {
    let header_parts = [
        render_prompt_profile_label(prompt.profile.as_deref()),
        render_prompt_tokens(prompt.total_input_tokens_est),
    ];
    let header = truncate_chars(
        &format!("Prompt    {}", header_parts.join(" · ")),
        PROMPT_PANEL_LINE_MAX_CHARS,
    );

    let mut lines = vec![header];

    if !prompt.enabled_layers.is_empty() {
        let names = prompt
            .enabled_layers
            .iter()
            .map(|name| escape_status_panel_markdown(name))
            .collect::<Vec<_>>()
            .join(", ");
        lines.push(truncate_chars(
            &format!("- 활성 ({}): {}", prompt.enabled_layers.len(), names),
            PROMPT_PANEL_LINE_MAX_CHARS,
        ));
    }

    if !prompt.skipped_layers.is_empty() {
        let parts: Vec<String> = prompt
            .skipped_layers
            .iter()
            .map(|entry| match entry.reason.as_deref() {
                Some(reason) => format!(
                    "{} ({})",
                    escape_status_panel_markdown(&entry.name),
                    truncate_chars(
                        &escape_status_panel_markdown(reason),
                        PROMPT_PANEL_SKIPPED_REASON_MAX_CHARS
                    )
                ),
                None => escape_status_panel_markdown(&entry.name),
            })
            .collect();
        lines.push(truncate_chars(
            &format!(
                "- 스킵 ({}): {}",
                prompt.skipped_layers.len(),
                parts.join(", ")
            ),
            PROMPT_PANEL_LINE_MAX_CHARS,
        ));
    }

    lines.join("\n")
}

fn render_prompt_profile_label(profile: Option<&str>) -> String {
    let Some(profile) = profile.map(str::trim).filter(|value| !value.is_empty()) else {
        return "Unknown profile".to_string();
    };
    match profile.to_ascii_lowercase().as_str() {
        "full" => "Full profile".to_string(),
        "lite" => "Lite profile".to_string(),
        "review_lite" | "review-lite" => "Review lite profile".to_string(),
        other => {
            let label = other.replace(['_', '-'], " ");
            let mut chars = label.chars();
            let mut rendered = match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => "Unknown".to_string(),
            };
            if !rendered.to_ascii_lowercase().ends_with(" profile") {
                rendered.push_str(" profile");
            }
            rendered
        }
    }
}

fn render_prompt_tokens(total_input_tokens_est: i64) -> String {
    format!(
        "~{:.1}k input tokens",
        (total_input_tokens_est.max(0) as f64) / 1000.0
    )
}
