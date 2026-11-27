use super::*;

#[test]
fn old_and_processing_intake_placeholders_share_exact_orphan_cleanup_guards() {
    let now = 1_700_000_000;
    let old = now - ORPHAN_PLACEHOLDER_MIN_AGE_SECS - 1;
    for content in ["...", "⠋ 계속 처리 중"] {
        let protected = HashSet::from([9001]);
        assert!(is_orphan_placeholder_candidate(
            7, 7, content, 9002, old, now, &protected
        ));
        for (author, message, changed) in [(8, 9002, old), (7, 9001, old), (7, 9002, old + 1)] {
            assert!(!is_orphan_placeholder_candidate(
                author, 7, content, message, changed, now, &protected
            ));
        }
    }
}

#[test]
fn intake_retirement_does_not_turn_prose_or_other_spinners_into_orphan_candidates() {
    let now = 1_700_000_000;
    let old = now - ORPHAN_PLACEHOLDER_MIN_AGE_SECS - 1;
    for content in [
        "…",
        " ... ",
        "⠋ 계속 처리 중\nanswer",
        "answer ⠋ 계속 처리 중",
        "⠙ 계속 처리 중",
    ] {
        assert!(!is_orphan_placeholder_candidate(
            7,
            7,
            content,
            9002,
            old,
            now,
            &HashSet::new()
        ));
    }
}

#[tokio::test]
async fn runtime_hygiene_startup_reclaim_keeps_uncertain_pending_start_owners() {
    use crate::services::discord::health::test_support::{MockDiscord, message_json};
    use crate::services::discord::{inflight, runtime_store, tui_direct_pending_start};
    use axum::http::Method;
    use std::fs;

    const CHANNEL: u64 = 23;
    const BOT: u64 = 7;
    const SNAPSHOT_OWNER: u64 = 101;
    const LATE_OWNER: u64 = 102;
    const UNREADABLE: u64 = 103;
    const MALFORMED: u64 = 104;
    const MISMATCHED: u64 = 105;
    const UNOWNED: u64 = 106;
    const OTHER_ANCHOR: u64 = 107;
    const ABORT_OWNER: u64 = 108;
    const QUARANTINED_ABORT: u64 = 109;
    const LATE_INFLIGHT: u64 = 110;
    const NOW: i64 = 1_700_000_000;

    let root = crate::config::TestRuntimeRootGuard::new();
    let provider = ProviderKind::Claude;
    let pending_root = runtime_store::tui_direct_pending_start_root().unwrap();
    fs::create_dir_all(&pending_root).unwrap();
    let path = |anchor| pending_root.join(format!("{}_{CHANNEL}_{anchor}.json", provider.as_str()));
    let record = |anchor| {
        serde_json::to_vec(&tui_direct_pending_start::TuiDirectPendingStart {
            provider: provider.as_str().into(),
            channel_id: CHANNEL,
            anchor_message_id: anchor,
            ..Default::default()
        })
        .unwrap()
    };
    fs::write(path(SNAPSHOT_OWNER), record(SNAPSHOT_OWNER)).unwrap();
    fs::create_dir(path(UNREADABLE)).unwrap();
    fs::write(path(MALFORMED), "{invalid json").unwrap();
    fs::write(path(MISMATCHED), record(OTHER_ANCHOR)).unwrap();
    let plan = reclaim_scan_plan_from_rows(
        Vec::new(),
        tui_direct_pending_start::load_all(),
        Vec::new(),
        &provider,
        NOW,
    );
    assert_eq!(
        plan.protected_message_ids,
        HashSet::from([SNAPSHOT_OWNER, OTHER_ANCHOR])
    );
    let abort_root = runtime_store::tui_direct_abort_marker_root().unwrap();
    fs::create_dir_all(&abort_root).unwrap();
    let abort_path =
        |anchor| abort_root.join(format!("{}_{CHANNEL}_{anchor}.json", provider.as_str()));
    let marker = serde_json::json!({
        "provider": provider.as_str(),
        "channel_id": CHANNEL,
        "anchor_message_id": ABORT_OWNER,
        "tmux_session_name": "test-session",
        "aborted_at_ms": NOW as u64 * 1000,
    });
    fs::write(
        abort_path(ABORT_OWNER),
        serde_json::to_vec(&marker).unwrap(),
    )
    .unwrap();
    fs::write(
        abort_path(QUARANTINED_ABORT).with_extension("json.bad"),
        "{invalid json",
    )
    .unwrap();

    let messages: Vec<_> = [
        SNAPSHOT_OWNER,
        LATE_OWNER,
        UNREADABLE,
        MALFORMED,
        MISMATCHED,
        ABORT_OWNER,
        QUARANTINED_ABORT,
        LATE_INFLIGHT,
        UNOWNED,
    ]
    .into_iter()
    .map(|id| message_json(id, CHANNEL, BOT, "...", "2020-01-01T00:00:00Z"))
    .collect();
    let late_path = path(LATE_OWNER);
    let late_record = record(LATE_OWNER);
    let row = inflight::InflightTurnState::new(
        provider.clone(),
        CHANNEL,
        None,
        0,
        0,
        LATE_INFLIGHT,
        "prompt".to_owned(),
        None,
        None,
        None,
        None,
        0,
    );
    let row_path = inflight::inflight_state_path(
        &runtime_store::discord_inflight_root().unwrap(),
        &provider,
        CHANNEL,
    );
    fs::create_dir_all(row_path.parent().unwrap()).unwrap();
    let row_bytes = serde_json::to_vec(&row).unwrap();
    let page = serde_json::Value::Array(messages);
    let channel_messages = format!("/channels/{CHANNEL}/messages");
    let discord = MockDiscord::start_with(Arc::new(move |method, path| {
        if method == Method::GET && path.ends_with(&channel_messages) {
            fs::write(&late_path, &late_record).unwrap();
            fs::write(&row_path, &row_bytes).unwrap();
            Some((200, page.clone()))
        } else {
            None
        }
    }))
    .await;

    let shared = crate::services::discord::make_shared_data_for_tests();
    let deleted = reclaim_orphan_placeholders(
        &discord.http,
        &shared,
        &provider,
        CHANNEL,
        BOT,
        NOW,
        &plan.protected_message_ids,
    )
    .await;

    assert!(
        path(LATE_OWNER).is_file(),
        "the page introduced a late owner"
    );
    let live_row = inflight::load_inflight_state_read_only_result(&provider, CHANNEL)
        .unwrap()
        .unwrap();
    assert_eq!(live_row.current_msg_id, LATE_INFLIGHT);
    let calls = discord.calls_for(CHANNEL);
    let attempted: Vec<_> = calls
        .iter()
        .filter_map(|call| call.strip_prefix("DELETE "))
        .map(|path| path.rsplit('/').next().unwrap().parse::<u64>().unwrap())
        .collect();
    assert_eq!(
        attempted,
        [UNOWNED],
        "only definitive absence permits DELETE"
    );
    assert_eq!(deleted, 1);
    drop(root);
}
