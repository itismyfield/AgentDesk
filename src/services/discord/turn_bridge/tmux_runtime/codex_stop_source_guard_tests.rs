//! Source guards for the `/stop` slash entry in `commands/control.rs`.

pub(super) fn act7_slash_stop_enters_through_run_slash_stop() {
    let source = include_str!("../../commands/control.rs");
    let body = source
        .split("async fn cmd_stop")
        .nth(1)
        .unwrap()
        .split("pub(super) fn parse_queued_message_id")
        .next()
        .unwrap();
    assert_eq!(body.matches("run_slash_stop(").count(), 1);
    assert!(!body.contains("begin_command_stop(") && !body.contains("begin_user_stop("));
}
