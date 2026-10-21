#[test]
fn c1b_off_role_override_keeps_runtime_transition_on_original_provider() {
    let source = include_str!("../intake_turn.rs");
    let handler_marker = ["\npub(super) async fn handle_", "text_message("].concat();
    let body = source.split_once(handler_marker.as_str()).unwrap().1;
    let start = body
        .find("    let Some((route, bootstrapped_fresh_thread_session))")
        .unwrap();
    let end = body[start..]
        .find("    let (mut session_id, mut memento_context_loaded")
        .unwrap()
        + start;
    let transition = &body[start..end];
    assert!(
        !transition.contains("let provider ="),
        "role override must not shadow the original native-clear provider"
    );
    assert!(
        transition.contains("(http, shared, token, &provider)"),
        "runtime transition and native-clear admission retain settings.provider"
    );
    let runtime = include_str!("runtime_transition.rs");
    assert!(runtime.contains("&mut t.recovered_fresh"));
}
