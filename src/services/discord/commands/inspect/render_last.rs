use super::formatting::{
    fenced_report, format_context_usage, format_duration, format_kst, format_prompt_summary,
    opt_or_none, push_kv, push_line, session_status_label, truncate_chars,
};
use super::model::{InspectContextConfig, LatestTurn, LifecycleEventRow};
use crate::db::prompt_manifests::PromptManifest;

pub(super) fn render_last_report(
    turn: &LatestTurn,
    session_event: Option<&LifecycleEventRow>,
    manifest: Option<&PromptManifest>,
    automation_events: &[LifecycleEventRow],
    context: &InspectContextConfig,
) -> String {
    let mut out = String::new();
    push_line(&mut out, "Last Turn");
    push_kv(&mut out, "turn_id", &turn.turn_id);
    push_kv(&mut out, "channel", &turn.channel_id);
    push_kv(&mut out, "provider", opt_or_none(turn.provider.as_deref()));
    push_kv(&mut out, "finished", &format_kst(turn.finished_at));
    push_kv(&mut out, "duration", &format_duration(turn.duration_ms));
    push_kv(
        &mut out,
        "session",
        session_event
            .map(session_status_label)
            .unwrap_or("(lifecycle 없음)"),
    );
    push_kv(&mut out, "context", &format_context_usage(turn, context));
    push_kv(&mut out, "prompt", &format_prompt_summary(manifest));
    push_kv(
        &mut out,
        "dispatch",
        opt_or_none(turn.dispatch_id.as_deref()),
    );
    push_line(&mut out, "");
    push_line(&mut out, "last automation:");
    if automation_events.is_empty() {
        push_line(&mut out, "- (없음)");
    } else {
        for event in automation_events.iter().take(5) {
            push_line(
                &mut out,
                &format!(
                    "- {} [{}] {}",
                    event.kind,
                    event.severity,
                    truncate_chars(&event.summary, 62)
                ),
            );
        }
    }
    fenced_report(out)
}
