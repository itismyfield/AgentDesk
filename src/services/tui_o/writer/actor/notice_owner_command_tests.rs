use super::*;

#[path = "notice_owner_absorbed_tests.rs"]
mod absorbed_tests;
#[path = "notice_owner_command_order_tests.rs"]
mod command_order_tests;

fn append_queued_command(c: &Case, frame: &str) {
    append_command(c, "physical-command", "original-input", frame);
}

fn append_command(c: &Case, physical_id: &str, input_id: &str, frame: &str) {
    let command = json!({
        "type": "attachment",
        "uuid": physical_id,
        "sessionId": "native",
        "attachment": {
            "type": "queued_command",
            "commandMode": "prompt",
            "origin": {"kind": "human"},
            "source_uuid": input_id,
            "prompt": frame
        }
    });
    append(&c.path, format!("{command}\n").as_bytes());
}

#[tokio::test(start_paused = true)]
async fn queued_command_receipt_excludes_the_notice_by_original_input_id() {
    let c = Case::new("queued-command-receipt");
    let frame = c.frame();
    let (registry, key) = c.v2(&frame, 8820, false);
    let mut actor = c.open();
    append_queued_command(&c, &frame);
    let fresh = actor.read_sources_for_test();
    assert_eq!(notice_evidence(&mut actor, &input_key("original-input")), 1);
    assert_eq!(
        registry.receipt(
            NAMESPACE,
            &key,
            (&c.path, 0, "command", Some("physical-command"), &frame)
        ),
        Some(true)
    );
    actor.prepare_notices_for_test(&fresh).await;
    c.excluded(&mut actor, "original-input");
}

#[tokio::test(start_paused = true)]
async fn original_user_receipt_owns_its_queued_command_notice() {
    for late_receipt in [false, true] {
        let c = Case::new("user-receipt-owns-command");
        let frame = c.frame();
        let (registry, key) = c.v2(&frame, 8821, false);
        let mut actor = c.open();
        append_queued_command(&c, &frame);
        if late_receipt {
            actor.read_sources_for_test();
            assert_eq!(notice_evidence(&mut actor, &input_key("original-input")), 1);
        }
        let offset = std::fs::metadata(&c.path).unwrap().len();
        append(&c.path, &row("original-input", &frame));
        assert_eq!(
            registry.receipt(
                NAMESPACE,
                &key,
                (&c.path, offset, "user", Some("original-input"), &frame)
            ),
            Some(true)
        );
        c.run(&mut actor).await;
        assert_eq!(c.notices(), 0);
        if late_receipt {
            c.excluded(&mut actor, "original-input");
        } else {
            assert_eq!(notice_evidence(&mut actor, &input_key("original-input")), 0);
        }
    }
}
