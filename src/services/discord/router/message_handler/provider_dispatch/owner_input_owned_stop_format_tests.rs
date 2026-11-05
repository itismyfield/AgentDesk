//! Native queue formats and compaction pass the owner admission and stop entries.
use super::*;

fn child(name: &str) -> bool {
    creator_child(&format!(
        "services::discord::router::message_handler::provider_dispatch::owner_input::tests::rollback::owned_stop::formats::{name}"
    ))
}

async fn queue_case(
    name: &str,
    id: u64,
    mode: &str,
    foreign: Option<&str>,
    duplicate: bool,
    expected: usize,
) {
    if !child(name) {
        return;
    }
    let root = crate::config::TestRuntimeRootGuard::new();
    let cutover = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    assert!(root.path().exists());
    let rig = StopRig::new(id, None).await;
    fs::write(rig.intake.renderer.join("queue-mode"), mode).unwrap();
    if let Some(marker) = foreign {
        fs::write(rig.intake.renderer.join(marker), "").unwrap();
    }
    if duplicate {
        fs::write(rig.intake.renderer.join("duplicate-enqueue"), "").unwrap();
        fs::write(rig.intake.renderer.join("duplicate-absorption"), "").unwrap();
    }
    rig.paste().await;
    rig.command().await;
    assert_eq!(
        rig.escapes(),
        expected,
        "{name}: only the exact absorbed owner input grants stop authority"
    );
    assert!(!rig.intake.mailbox.has_active_turn().await.unwrap());
    drop(rig);
    drop(cutover);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remove_only_owner_input_stops_its_open_turn() {
    queue_case(
        "remove_only_owner_input_stops_its_open_turn",
        7755300,
        "remove-only",
        None,
        false,
        1,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreign_absorption_before_owner_input_blocks_stop() {
    queue_case(
        "foreign_absorption_before_owner_input_blocks_stop",
        7755301,
        "command-after",
        Some("foreign-absorbed-before"),
        false,
        0,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreign_absorption_after_owner_input_blocks_stop() {
    queue_case(
        "foreign_absorption_after_owner_input_blocks_stop",
        7755302,
        "command-before",
        Some("foreign-absorbed-after"),
        false,
        0,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_command_before_absorption_consumes_one_input() {
    queue_case(
        "a_command_before_absorption_consumes_one_input",
        7755303,
        "command-before",
        None,
        false,
        1,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_command_after_absorption_consumes_one_input() {
    queue_case(
        "a_command_after_absorption_consumes_one_input",
        7755304,
        "command-after",
        None,
        false,
        1,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_body_inputs_are_not_borrowed_by_command_before_absorption() {
    queue_case(
        "same_body_inputs_are_not_borrowed_by_command_before_absorption",
        7755305,
        "command-before",
        None,
        true,
        0,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_body_inputs_are_not_borrowed_by_command_after_absorption() {
    queue_case(
        "same_body_inputs_are_not_borrowed_by_command_after_absorption",
        7755306,
        "command-after",
        None,
        true,
        0,
    )
    .await;
}

async fn summary_case(
    name: &str,
    id: u64,
    field: &str,
    value: Value,
    token: bool,
    expected: usize,
    foreign_sid: bool,
) {
    if !child(name) {
        return;
    }
    let root = crate::config::TestRuntimeRootGuard::new();
    let cutover = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    assert!(root.path().exists());
    let rig = StopRig::new(id, None).await;
    if token {
        fs::remove_file(rig.intake.renderer.join("queued-row")).unwrap();
    }
    rig.paste().await;
    let active = if token {
        Some(rig.own_user_claim().await)
    } else {
        None
    };
    append(
        &rig.intake.transcript,
        json!({"type":"system","subtype":"compact_boundary","compactMetadata":{"trigger":"auto"},"sessionId":rig.sid}),
    );
    let sid = if foreign_sid {
        "another-native-session"
    } else {
        rig.sid.as_str()
    };
    let mut summary = json!({"type":"user","uuid":"compact-summary","sessionId":sid,"message":{"role":"user","content":"native automatic continuation summary"}});
    summary[field] = value;
    append(&rig.intake.transcript, summary);
    append(
        &rig.intake.transcript,
        json!({"type":"assistant","uuid":"post-compact-work","sessionId":rig.sid,"apiBlockIndex":0,"message":{"id":"post-compact-work","content":[{"type":"text","text":"continuing the same work"}]}}),
    );
    rig.command().await;
    assert_eq!(
        rig.escapes(),
        expected,
        "{name}: only strict same-session compact summaries are neutral"
    );
    if let Some(active) = active {
        assert!(
            !active.cancelled.load(Ordering::Acquire),
            "native stop never raw cancels"
        );
    }
    drop(rig);
    drop(cutover);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compact_summary_keeps_the_owned_native_root() {
    summary_case(
        "compact_summary_keeps_the_owned_native_root",
        7755400,
        "isCompactSummary",
        json!(true),
        true,
        1,
        false,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn underscored_summary_keeps_the_owned_native_root() {
    summary_case(
        "underscored_summary_keeps_the_owned_native_root",
        7755401,
        "is_compact_summary",
        json!(true),
        true,
        1,
        false,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compact_summary_keeps_tokenless_owned_input() {
    summary_case(
        "compact_summary_keeps_tokenless_owned_input",
        7755402,
        "isCompactSummary",
        json!(true),
        false,
        1,
        false,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn underscored_summary_keeps_tokenless_owned_input() {
    summary_case(
        "underscored_summary_keeps_tokenless_owned_input",
        7755403,
        "is_compact_summary",
        json!(true),
        false,
        1,
        false,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn false_summary_does_not_exempt_a_foreign_user() {
    summary_case(
        "false_summary_does_not_exempt_a_foreign_user",
        7755404,
        "isCompactSummary",
        json!(false),
        true,
        0,
        false,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn string_summary_does_not_exempt_a_foreign_user() {
    summary_case(
        "string_summary_does_not_exempt_a_foreign_user",
        7755405,
        "is_compact_summary",
        json!("true"),
        true,
        0,
        false,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_summary_from_another_session_is_foreign() {
    summary_case(
        "a_summary_from_another_session_is_foreign",
        7755406,
        "isCompactSummary",
        json!(true),
        true,
        0,
        true,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ordinary_foreign_user_remains_foreign() {
    summary_case(
        "an_ordinary_foreign_user_remains_foreign",
        7755407,
        "unrelated",
        json!(true),
        true,
        0,
        false,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_nonhuman_command_does_not_hide_a_foreign_absorption() {
    const NAME: &str = "a_nonhuman_command_does_not_hide_a_foreign_absorption";
    if !child(NAME) {
        return;
    }
    let root = crate::config::TestRuntimeRootGuard::new();
    let cutover = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    assert!(root.path().exists());
    let rig = StopRig::new(7_755_420, None).await;
    rig.paste().await;
    for operation in ["enqueue", "remove"] {
        append(
            &rig.intake.transcript,
            json!({"type":"queue-operation","operation":operation,"reason":"absorbed_mid_turn","content":"foreign text","sessionId":rig.sid}),
        );
    }
    append(
        &rig.intake.transcript,
        json!({"type":"attachment","uuid":"agent-notification","sessionId":rig.sid,"attachment":{"type":"queued_command","commandMode":"prompt","origin":{"kind":"agent"},"prompt":"foreign text"}}),
    );
    rig.command().await;
    assert_eq!(
        rig.escapes(),
        0,
        "an unrelated same-body agent command never neutralizes human absorption"
    );
    drop(rig);
    drop(cutover);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_foreign_absorption_cannot_borrow_the_pinned_user_frame() {
    const NAME: &str = "a_foreign_absorption_cannot_borrow_the_pinned_user_frame";
    if !child(NAME) {
        return;
    }
    let root = crate::config::TestRuntimeRootGuard::new();
    let cutover = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    assert!(root.path().exists());
    let rig = StopRig::new(7_755_421, None).await;
    fs::remove_file(rig.intake.renderer.join("queued-row")).unwrap();
    rig.paste().await;
    let token = rig.own_user_claim().await;
    let body = fs::read_to_string(rig.intake.renderer.join("body")).unwrap();
    for operation in ["enqueue", "remove"] {
        append(
            &rig.intake.transcript,
            json!({"type":"queue-operation","operation":operation,"reason":"absorbed_mid_turn","content":body,"sessionId":rig.sid}),
        );
    }
    append(
        &rig.intake.transcript,
        json!({"type":"assistant","sessionId":rig.sid,"message":{"content":[{"type":"text","text":"still working"}]},"apiBlockIndex":0}),
    );
    rig.command().await;
    assert_eq!(
        rig.escapes(),
        0,
        "a registration before the pinned root cannot own a later enqueue"
    );
    assert!(!token.cancelled.load(Ordering::Acquire));
    drop(rig);
    drop(cutover);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_summary_repeating_the_frame_does_not_consume_the_attempt() {
    const NAME: &str = "a_summary_repeating_the_frame_does_not_consume_the_attempt";
    if !child(NAME) {
        return;
    }
    let root = crate::config::TestRuntimeRootGuard::new();
    let cutover = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    assert!(root.path().exists());
    let rig = StopRig::new(7_755_422, Some("compact-before-command")).await;
    rig.paste().await;
    use crate::services::tui_input::human_frame::history::{self, RowOwner};
    let body = fs::read_to_string(rig.intake.renderer.join("body")).unwrap();
    assert!(
        matches!(
            history::row_owner("claude", &rig.session, &[body], "owned-busy-row"),
            RowOwner::Command(_)
        ),
        "the summary cannot take the real command's exact attempt"
    );
    append(
        &rig.intake.transcript,
        json!({"type":"assistant","uuid":"summary-work-continued","sessionId":rig.sid,"apiBlockIndex":0,"message":{"id":"summary-work-continued","content":[{"type":"text","text":"work continues after compaction"}]}}),
    );
    rig.command().await;
    assert_eq!(
        rig.escapes(),
        1,
        "a compact summary never consumes the real command's registration"
    );
    drop(rig);
    drop(cutover);
}

async fn identical_admissions(name: &str, deferred: bool) {
    if !child(name) {
        return;
    }
    let root = crate::config::TestRuntimeRootGuard::new();
    let cutover = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let rig = StopRig::new(7_755_423, None).await;
    fs::write(rig.intake.renderer.join("queue-mode"), "remove-only").unwrap();
    if deferred {
        fs::write(rig.intake.renderer.join("defer-two-enqueues"), "").unwrap();
        let (drive, mut settled) = rig.intake.drive();
        crate::services::discord::queue_io::input_owner::drive::wake(drive);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(30), settled.recv())
                .await
                .unwrap()
                .unwrap(),
            Ok(())
        );
        wait("the first Enter is held for the second admission", || {
            events(&rig.intake.renderer) == ["paste", "Enter"]
        });
    } else {
        rig.paste().await;
    }
    let first_body = fs::read_to_string(rig.intake.renderer.join("body")).unwrap();
    use crate::services::discord::queue_io::input_owner::admission::{
        Admission, AdmissionTicket, admit,
    };
    let original: crate::services::turn_orchestrator::PendingQueueItem = serde_json::from_value(json!({"author_id":7,"message_id":1_479_671_302_387_077_000u64+rig.intake.channel.get(),"channel_id":rig.intake.channel.get(),"queued_generation":1,"text":PROMPT})).unwrap();
    assert!(matches!(
        admit(&rig.intake.mailbox, &original, AdmissionTicket::new()).await,
        Admission::Accepted(_)
    ));
    fs::write(
        rig.intake.renderer.join("native-uuid"),
        "owned-busy-row-second",
    )
    .unwrap();
    rig.paste().await;
    assert_eq!(
        first_body,
        fs::read_to_string(rig.intake.renderer.join("body")).unwrap(),
        "two independent Discord admissions pasted the same exact frame"
    );
    let source = fs::read_to_string(&rig.intake.transcript).unwrap();
    let mut offset = 0u64;
    let mut enqueues = Vec::new();
    for line in source.split_inclusive('\n') {
        let row: Value = serde_json::from_str(line).unwrap();
        if row["type"] == "queue-operation" && row["operation"] == "enqueue" {
            enqueues.push((offset, row["content"].as_str().unwrap().to_owned()));
        }
        offset += line.len() as u64;
    }
    assert_eq!(enqueues.len(), 2);
    use crate::services::tui_input::human_frame::history::{self, RowOwner};
    let owners = history::discord_enqueue_owners(
        &rig.session,
        &rig.sid,
        &rig.intake.transcript,
        rig.opener_end,
        &enqueues,
        &[],
    )
    .unwrap();
    let ledger: Value = serde_json::from_slice(
        &fs::read(root.path().join("runtime/injected_frames.json")).unwrap(),
    )
    .unwrap();
    let mut registered: Vec<_> = ledger["live"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|entry| &entry["registration"])
        .chain(
            ledger["passive"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|entry| !entry["entry"].is_null())
                .map(|entry| &entry["registration"]),
        )
        .filter(|registration| registration["source"]["target"]["owner"] == rig.session)
        .map(|registration| {
            (
                registration["source"]["offset"].as_u64().unwrap(),
                registration["key"]["attempt"]["nonce"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
            )
        })
        .collect();
    registered.sort();
    assert_eq!(registered.len(), 2);
    if deferred {
        assert_eq!(
            registered[0].0, registered[1].0,
            "both actual writes preceded the native queue records"
        );
        assert_eq!(
            owners,
            vec![RowOwner::Unread, RowOwner::Unread],
            "an offset tie never swaps live/passive attempts"
        );
    } else {
        assert!(registered[0].0 < registered[1].0);
        assert_eq!(
            owners,
            registered
                .iter()
                .map(|(_, id)| RowOwner::Command(id.clone()))
                .collect::<Vec<_>>(),
            "same frames keep each exact registration and consume it once"
        );
    }
    rig.command().await;
    assert_eq!(
        rig.escapes(),
        usize::from(!deferred),
        "only unambiguous exact attempts authorize the stop"
    );
    drop(rig);
    drop(cutover);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn identical_frames_keep_two_independent_admissions() {
    identical_admissions("identical_frames_keep_two_independent_admissions", false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn identical_registration_offsets_remain_unread() {
    identical_admissions("identical_registration_offsets_remain_unread", true).await;
}
