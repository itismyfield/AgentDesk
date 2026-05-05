use super::formatting::{
    adk_session_from_event, fenced_report, format_kst, opt_or_none, push_kv, push_line,
    session_id_from_event, session_status_label, tmux_action_label,
};
use super::model::{LatestTurn, LifecycleEventRow};

pub(super) fn render_session_report(
    turn: &LatestTurn,
    event: Option<&LifecycleEventRow>,
) -> String {
    let mut out = String::new();
    push_line(&mut out, "Session");
    push_kv(
        &mut out,
        "status",
        event
            .map(session_status_label)
            .unwrap_or("(lifecycle 없음)"),
    );
    push_kv(&mut out, "provider", opt_or_none(turn.provider.as_deref()));
    let provider_session = session_id_from_event(event)
        .map(str::to_string)
        .unwrap_or_else(|| opt_or_none(turn.session_id.as_deref()));
    push_kv(&mut out, "provider session", provider_session);
    let adk_session = adk_session_from_event(event)
        .map(str::to_string)
        .unwrap_or_else(|| opt_or_none(turn.session_key.as_deref()));
    push_kv(&mut out, "adk session", adk_session);
    push_kv(&mut out, "backend", "tmux");
    push_kv(&mut out, "last clear reason", "(없음)");
    push_kv(&mut out, "last tmux action", &tmux_action_label(event));
    if let Some(event) = event {
        push_kv(&mut out, "event at", &format_kst(event.created_at));
        push_kv(&mut out, "event summary", &event.summary);
    }
    fenced_report(out)
}
