use crate::services::discord::router::source_guard_support::Lexed;

#[test]
fn intake_turn_keeps_retry_carriers_at_each_production_handoff() {
    let source = Lexed::new(include_str!("../../intake_turn.rs"));
    let handler = source
        .block("pub(super) async fn handle_text_message(")
        .unwrap();
    let rendered = handler
        .find("let reply_context = prepared_prompt.rendered.clone();")
        .unwrap();
    let prompt = handler
        .find_after(rendered, "reply: reply_context.as_deref(),")
        .unwrap();
    let busy = handler
        .find_after(
            prompt,
            "let enqueue_outcome = enqueue_busy_tui_followup_for_retry(",
        )
        .unwrap();
    let snapshot = handler
        .find_after(busy, "inflight_state.set_followup_requeue_context(")
        .unwrap();
    assert!(
        handler
            .slice(busy..snapshot)
            .contains("prepared_prompt.context.requeue(),"),
        "INTAKE_RETRY_CARRIER_WIRING early busy must return raw carrier"
    );
    let replay = handler
        .find_after(
            snapshot,
            "let replay_request = replay_start::StartRequest {",
        )
        .unwrap();
    assert!(
        handler
            .slice(snapshot..replay)
            .contains("prepared_prompt.snapshot(&context_prompt),"),
        "INTAKE_RETRY_CARRIER_WIRING inflight snapshot"
    );
    assert!(
        handler
            .slice(snapshot..replay)
            .contains("let replay_reply_context = prepared_prompt.context.requeue();"),
        "INTAKE_RETRY_CARRIER_WIRING canonical replay carrier"
    );
    assert!(
        handler
            .slice(replay..handler.text().len())
            .contains("reply_context: replay_reply_context.as_deref(),"),
        "INTAKE_RETRY_CARRIER_WIRING canonical replay consumer"
    );
}
