use super::formatting::{
    fenced_report, format_compaction, format_context_usage, format_tokens, opt_or_none, push_kv,
    push_line, truncate_chars,
};
use super::model::{InspectContextConfig, LatestTurn, LifecycleEventRow};
use crate::db::prompt_manifests::PromptManifest;

pub(super) fn render_context_report(
    turn: &LatestTurn,
    manifest: Option<&PromptManifest>,
    compaction: Option<&LifecycleEventRow>,
    context: &InspectContextConfig,
) -> String {
    let mut out = String::new();
    push_line(&mut out, "Context Window");
    push_kv(&mut out, "turn_id", &turn.turn_id);
    push_kv(&mut out, "usage", &format_context_usage(turn, context));
    push_kv(
        &mut out,
        "auto-compact threshold",
        &format!("{}%", context.compact_percent),
    );
    push_kv(&mut out, "provider", context.provider.as_str());
    push_kv(&mut out, "model", opt_or_none(context.model.as_deref()));
    match manifest {
        Some(manifest) => {
            push_kv(
                &mut out,
                "prompt estimate",
                &format!("{} tokens", format_tokens(manifest.total_input_tokens_est)),
            );
        }
        None => {
            push_kv(
                &mut out,
                "prompt estimate",
                "(manifest pending for this turn)",
            );
        }
    }
    push_kv(&mut out, "last compact", &format_compaction(compaction));
    push_line(&mut out, "");
    match manifest {
        Some(manifest) => {
            let mut layers = manifest.layers.clone();
            layers.sort_by(|a, b| b.tokens_est.cmp(&a.tokens_est));
            push_line(&mut out, "largest layers:");
            for layer in layers.iter().take(6) {
                push_line(
                    &mut out,
                    &format!(
                        "- {}: {}",
                        truncate_chars(&layer.layer_name, 54),
                        format_tokens(layer.tokens_est)
                    ),
                );
            }
        }
        None => {
            push_line(&mut out, "largest layers:");
            push_line(&mut out, "- (manifest pending for this turn)");
        }
    }
    fenced_report(out)
}
