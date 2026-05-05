use super::formatting::{
    REPORT_LINE_MAX, fenced_report, format_tokens, opt_or_none, push_kv, push_line, truncate_chars,
    visibility_label, wrap_line,
};
use crate::db::prompt_manifests::{PromptContentVisibility, PromptManifest, PromptManifestLayer};

const SOURCE_MAX_CHARS: usize = 30;
const PROMPT_LAYER_CONTENT_MAX_CHARS: usize = 360;
const PROMPT_LAYER_CONTENT_MAX_LAYERS: usize = 6;

pub(super) fn render_prompt_manifest_report(manifest: &PromptManifest) -> String {
    let mut out = String::new();
    push_line(
        &mut out,
        &format!(
            "Prompt Manifest {}",
            manifest
                .id
                .map(|id| format!("pm_{id}"))
                .unwrap_or_else(|| "(unsaved)".to_string())
        ),
    );
    push_kv(&mut out, "turn_id", &manifest.turn_id);
    push_kv(
        &mut out,
        "profile",
        opt_or_none(manifest.profile.as_deref()),
    );
    push_kv(
        &mut out,
        "total input estimate",
        &format!("{} tokens", format_tokens(manifest.total_input_tokens_est)),
    );
    push_kv(
        &mut out,
        "layers",
        &format!(
            "{}/{} enabled",
            manifest.layers.iter().filter(|layer| layer.enabled).count(),
            manifest.layers.len()
        ),
    );
    push_line(&mut out, "");
    push_line(&mut out, "Layers");
    for layer in &manifest.layers {
        push_line(&mut out, &format_prompt_layer_summary(layer));
    }

    push_line(&mut out, "");
    push_line(
        &mut out,
        "Layer content (ADK full source, user redacted preview)",
    );
    for layer in manifest
        .layers
        .iter()
        .filter(|layer| layer.enabled)
        .take(PROMPT_LAYER_CONTENT_MAX_LAYERS)
    {
        push_line(
            &mut out,
            &format!(
                "{} [{}]",
                truncate_chars(&layer.layer_name, 38),
                visibility_label(layer.content_visibility)
            ),
        );
        for line in layer_display_body(layer)
            .lines()
            .flat_map(|line| wrap_line(line, REPORT_LINE_MAX - 2))
            .take(6)
        {
            push_line(&mut out, &format!("  {line}"));
        }
    }
    fenced_report(out)
}

fn format_prompt_layer_summary(layer: &PromptManifestLayer) -> String {
    let marker = if layer.enabled { "+" } else { "-" };
    let source = layer
        .source
        .as_deref()
        .or(layer.reason.as_deref())
        .map(|value| truncate_chars(value, SOURCE_MAX_CHARS))
        .unwrap_or_else(|| "(source 없음)".to_string());
    format!(
        "{marker} {:<24} {:<30} {:>7} {}",
        truncate_chars(&layer.layer_name, 24),
        source,
        format_tokens(layer.tokens_est),
        visibility_label(layer.content_visibility)
    )
}

fn layer_display_body(layer: &PromptManifestLayer) -> String {
    let raw = match layer.content_visibility {
        PromptContentVisibility::AdkProvided => layer
            .full_content
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or("(content 없음)"),
        PromptContentVisibility::UserDerived => layer
            .redacted_preview
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or("(redacted preview 없음)"),
    };
    truncate_chars(raw.trim(), PROMPT_LAYER_CONTENT_MAX_CHARS)
}
