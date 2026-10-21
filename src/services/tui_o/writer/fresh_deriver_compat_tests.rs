//! Fresh source selection preserves the production deriver's cards and pending summaries.

use super::*;
use crate::services::tui_o::store::tool_card_switch;
use crate::services::tui_o::writer::pieces::{TOOL_CALL_CARD, TOOL_RESULT_INTERNAL};

fn native(path: &Path, value: serde_json::Value) {
    append(path, format!("{value}\n").as_bytes());
}

fn started(path: &Path) {
    native(
        path,
        serde_json::json!({"type":"event_msg", "timestamp":"2026-10-20T00:00:00Z",
        "payload":{"type":"task_started", "turn_id":"scope-native-turn"}}),
    );
}

fn tool(path: &Path) {
    native(
        path,
        serde_json::json!({"type":"response_item", "payload": {
        "type":"function_call", "id":"scope-native-tool", "call_id":"scope-native-call",
        "name":"functions.exec_command", "arguments":"{\"cmd\":\"cargo check\"}"}}),
    );
}

fn result(path: &Path) {
    native(
        path,
        serde_json::json!({"type":"response_item", "payload": {
        "type":"function_call_output", "call_id":"scope-native-call",
        "output":"PRIVATE_SCOPE_TOOL_RESULT"}}),
    );
}

fn body(path: &Path, id: &str, text: &str) {
    native(
        path,
        serde_json::json!({"type":"response_item", "payload": {
        "type":"message", "role":"assistant", "id":id,
        "content":[{"type":"output_text", "text":text}]}}),
    );
}

fn ended(path: &Path) {
    native(
        path,
        serde_json::json!({"type":"event_msg", "timestamp":"2026-10-20T00:00:07Z",
        "payload":{"type":"task_complete", "turn_id":"scope-native-turn"}}),
    );
}

fn key(native: &str, kind: UnitKind) -> UnitKey {
    UnitKey {
        channel_id: CANARY_CHANNEL,
        provider: ShadowProvider::Codex,
        native_key: native.into(),
        kind,
    }
}

fn initial(source: &SourceId) -> Harness {
    let harness = Harness::build_channel(CANARY_CHANNEL, |_| {
        vec![InitSource {
            source_id: source.clone(),
            delivery_start: std::fs::metadata(&source.path).unwrap().len(),
            prefix_hash: hex::encode(Sha256::digest(std::fs::read(&source.path).unwrap())),
        }]
    });
    harness.gate.acquired();
    let checkpoint = dedupe::binding_events::binding_events_since(CANARY_CHANNEL, 0)
        .unwrap()
        .last()
        .unwrap()
        .seq;
    harness
        .channel()
        .set_binding_checkpoint(checkpoint)
        .unwrap();
    harness
}

fn cards_on(root: &Path) {
    let mut config = crate::config_live_reload::current()
        .map(|config| (*config).clone())
        .unwrap_or_default();
    config.tui_o = Some(crate::services::tui_o::shadow::tap::TuiOConfig::default());
    crate::config_live_reload::install(config);
    assert_eq!(crate::config::runtime_root().as_deref(), Some(root));
    tool_card_switch::set(root, CANARY_CHANNEL, true, 7163).unwrap();
    assert!(tool_card_switch::global_enabled());
    assert!((tool_card_switch::ToolCardSwitch::for_channel(
        CANARY_CHANNEL
    ))());
    crate::services::tui_prompt_dedupe::codex_verified::provenance::admission::set_ready_for_tests(
        CANARY_CHANNEL,
        false,
    );
    assert!(!crate::services::tui_prompt_dedupe::codex_verified::provenance::admission::owed_delivery_ready(CANARY_CHANNEL), "use production FIFO readiness for native cards");
}

#[tokio::test(start_paused = true)]
async fn fresh_proof_preserves_tool_cards_and_turn_summary_after_deriver_reset() {
    if !crate::services::tui_o::cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::fresh_proof_preserves_tool_cards_and_turn_summary_after_deriver_reset"
    )) {
        return;
    }
    let _live = crate::config_live_reload::test_live_guard(
        crate::config::test_env_lock::acquire_shared_test_env_lock(),
    );
    let _dedupe = DedupeGuard::acquire();
    let (root, _env) = dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
    let _scope = ScopeFixture::new();
    let _tmux =
        crate::services::provider_teardown::tests::test_support::FakeTmux::install(CANARY_TMUX);
    dedupe::reset_state_for_tests();
    dedupe::binding_events::set_test_root(Some(root.path()));
    cards_on(root.path());
    let (_, n2) = proven(root.path());
    let harness = initial(&n2);
    let writer = harness.writer();
    let selection = writer.suspension.clone();
    let (stop, stopped) = watch::channel(false);
    let (resumed, ready) = watch::channel(false);
    let actor = tokio::spawn(run_channel(
        writer,
        ShadowProvider::Codex,
        Arc::new(ChannelBindingLog::new(
            CANARY_CHANNEL,
            ShadowProvider::Codex,
        )),
        stopped,
        resumed,
    ));
    polls(3).await;
    assert!(*ready.borrow(), "N2 must resume before the fresh N3 proof");
    let before = harness.channel().cursor(&n2).unwrap().clone();
    let bytes = std::fs::read(&n2.path).unwrap();
    let (n3_context, n3) = proven(root.path());
    let old_io = crate::services::codex::actual_submit_tests::source_io::Watch::start(&n2.path);
    polls(4).await;
    assert!(
        selection.contains(&n2),
        "N3 proof must perform the nonempty scope reset"
    );
    assert_eq!(
        selection.nonce().as_deref(),
        Some(n3_context.execution_nonce.as_str())
    );
    started(&n3.path);
    tool(&n3.path);
    result(&n3.path);
    body(&n3.path, "scope-fresh-body", "fresh tool answer");
    ended(&n3.path);
    polls(8).await;
    let posts = harness.port.posts();
    let alarms = harness.alarms.taken();
    stop.send_replace(true);
    assert_eq!(actor.await.unwrap(), None);
    assert_eq!(
        posts,
        [
            "-# ▸ Bash  cargo check  ·",
            "fresh tool answer",
            "-# ✅ 7초"
        ],
        "fresh N3 deriver reset must keep tool cards and turn summaries enabled: {alarms:?}"
    );
    assert!(
        !posts
            .iter()
            .any(|post| post.contains("PRIVATE_SCOPE_TOOL_RESULT"))
    );
    let store = harness.channel();
    let ledger = store.ledger();
    let native_tool = key("scope-native-tool", UnitKind::Tool);
    assert_eq!(ledger.excluded(&native_tool), Some(TOOL_CALL_CARD));
    assert!(ledger.card_tool_keys().contains(&native_tool));
    assert_eq!(
        ledger.excluded(&key("scope-native-call", UnitKind::ToolResult)),
        Some(TOOL_RESULT_INTERNAL)
    );
    assert!(
        ledger
            .summary_turn_marks()
            .contains("scope-native-tool#turn")
    );
    let card = ledger
        .latest_piece(&key("scope-native-tool#card", UnitKind::Tool), 0)
        .unwrap()
        .1;
    assert_eq!(card.payload, posts[0]);
    assert!(matches!(card.outcome, Some(PieceOutcome::Posted(_))));
    let summary = ledger
        .latest_piece(&key("scope-native-tool#turn_end", UnitKind::Tool), 0)
        .unwrap()
        .1;
    assert_eq!(summary.payload, posts[2]);
    assert!(matches!(summary.outcome, Some(PieceOutcome::Posted(_))));
    assert_eq!(store.cursor(&n2), Some(&before));
    assert_eq!(std::fs::read(&n2.path).unwrap(), bytes);
    assert_eq!(old_io.stats().read_bytes, 0);
    dedupe::binding_events::forget_channel_for_tests(CANARY_CHANNEL);
    dedupe::binding_events::set_test_root(None);
    dedupe::reset_state_for_tests();
}

#[derive(Clone, Copy, Debug)]
enum Pending {
    ToolCard,
    Summary,
    StartedOnly,
}

async fn pending_deriver_holds(case: Pending) {
    let _live = crate::config_live_reload::test_live_guard(
        crate::config::test_env_lock::acquire_shared_test_env_lock(),
    );
    let _dedupe = DedupeGuard::acquire();
    let (root, _env) = dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
    let _scope = ScopeFixture::new();
    let _tmux =
        crate::services::provider_teardown::tests::test_support::FakeTmux::install(CANARY_TMUX);
    dedupe::reset_state_for_tests();
    dedupe::binding_events::set_test_root(Some(root.path()));
    cards_on(root.path());
    let (n3_context, n3) = proven(root.path());
    let harness = initial(&n3);
    let writer = harness.writer();
    let selection = writer.suspension.clone();
    let (stop, stopped) = watch::channel(false);
    let (resumed, ready) = watch::channel(false);
    let actor = tokio::spawn(run_channel(
        writer,
        ShadowProvider::Codex,
        Arc::new(ChannelBindingLog::new(
            CANARY_CHANNEL,
            ShadowProvider::Codex,
        )),
        stopped,
        resumed,
    ));
    polls(3).await;
    assert!(*ready.borrow());
    assert!(
        selection.is_empty(),
        "initial N3 must retain the production deriver builder"
    );
    started(&n3.path);
    let first = match case {
        Pending::ToolCard => {
            tool(&n3.path);
            "scope-native-tool"
        }
        Pending::Summary => {
            body(&n3.path, "scope-pending-body", "current pending summary");
            "scope-pending-body"
        }
        Pending::StartedOnly => "",
    };
    polls(6).await;
    assert_eq!(
        crate::services::tui_o::writer::actor::work_for_tests(),
        (0, false),
        "{case:?}: no FIFO work or seal obligation may hide the deriver-only pending state"
    );
    let before = harness.channel().cursor(&n3).unwrap().clone();
    let bytes = std::fs::read(&n3.path).unwrap();
    assert_eq!(before.captured_through, bytes.len() as u64);
    let rotation = harness.channel().rotation().unwrap();
    let posted_before = harness.port.posts();
    assert_eq!(
        posted_before.len(),
        usize::from(matches!(case, Pending::Summary))
    );
    {
        let store = harness.channel();
        let ledger = store.ledger();
        if matches!(case, Pending::StartedOnly) {
            assert!(
                ledger.summary_turn_marks().is_empty(),
                "task_started alone has no first eligible summary unit"
            );
        } else {
            assert!(
                ledger
                    .summary_turn_marks()
                    .contains(&format!("{first}#turn")),
                "the open turn must have a first eligible summary unit"
            );
        }
        if matches!(case, Pending::ToolCard) {
            assert_eq!(
                ledger.excluded(&key(first, UnitKind::Tool)),
                Some(TOOL_CALL_CARD)
            );
            assert!(
                ledger
                    .latest_piece(&key("scope-native-tool#card", UnitKind::Tool), 0)
                    .is_none()
            );
        } else if matches!(case, Pending::Summary) {
            assert_eq!(posted_before, ["current pending summary"]);
            assert!(
                ledger
                    .latest_piece(&key("scope-pending-body#turn_end", UnitKind::Tool), 0)
                    .is_none()
            );
        }
    }
    let (n4_context, n4) = proven(root.path());
    assert_ne!(n4_context.execution_nonce, n3_context.execution_nonce);
    let old_io = crate::services::codex::actual_submit_tests::source_io::Watch::start(&n3.path);
    let fresh_body = if matches!(case, Pending::StartedOnly) {
        "new root after empty N3 turn"
    } else {
        "new root must wait for the pending deriver"
    };
    answer(&n4, fresh_body);
    polls(6).await;
    let posts = harness.port.posts();
    let alarms = harness.alarms.taken();
    stop.send_replace(true);
    assert_eq!(actor.await.unwrap(), None);
    if matches!(case, Pending::StartedOnly) {
        assert_eq!(
            selection.nonce().as_deref(),
            Some(n4_context.execution_nonce.as_str()),
            "task_started without a first eligible summary unit must allow same-actor N4 progress"
        );
        assert!(selection.contains(&n3));
        assert_eq!(
            posts,
            [fresh_body, "-# ✅ 완료"],
            "same-actor N4 must POST its Q1 and normal summary after an empty N3 turn"
        );
        assert_eq!(
            alarms
                .iter()
                .filter(|alarm| matches!(alarm,
            WriterAlarm::WaitingTooLong { incident, .. }
                if incident.starts_with("new_root_waiting:")))
                .count(),
            0,
            "an empty summary turn must not raise a scope-work hold alarm: {alarms:?}"
        );
    } else {
        assert_eq!(
            selection.nonce().as_deref(),
            Some(n3_context.execution_nonce.as_str()),
            "{case:?}: pending card or summary state must hold fresh root rescope"
        );
        assert!(
            selection.is_empty(),
            "{case:?}: a held root must not suspend its pending predecessor"
        );
        assert_eq!(
            posts, posted_before,
            "{case:?}: a pending deriver may not grant a successor POST"
        );
        assert_eq!(
            alarms.iter().filter(|alarm| matches!(alarm,
                WriterAlarm::WaitingTooLong { incident, .. } if incident.starts_with("new_root_waiting:"))).count(),
            1,
            "{case:?}: an exact new root blocked by pending derivation raises one alarm: {alarms:?}"
        );
    }
    let store = harness.channel();
    assert_eq!(store.cursor(&n3), Some(&before));
    if !matches!(case, Pending::StartedOnly) {
        assert_eq!(store.rotation().unwrap(), rotation);
    }
    assert_eq!(std::fs::read(&n3.path).unwrap(), bytes);
    assert_eq!(old_io.stats().read_bytes, 0);
    dedupe::binding_events::forget_channel_for_tests(CANARY_CHANNEL);
    dedupe::binding_events::set_test_root(None);
    dedupe::reset_state_for_tests();
}

#[tokio::test(start_paused = true)]
async fn pending_card_and_summary_hold_new_proof_without_suspending_their_source() {
    if !crate::services::tui_o::cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::pending_card_and_summary_hold_new_proof_without_suspending_their_source"
    )) {
        return;
    }
    pending_deriver_holds(Pending::ToolCard).await;
    pending_deriver_holds(Pending::Summary).await;
}

#[tokio::test(start_paused = true)]
async fn task_started_without_first_eligible_summary_allows_same_actor_fresh_q1() {
    if crate::services::tui_o::cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::task_started_without_first_eligible_summary_allows_same_actor_fresh_q1"
    )) {
        pending_deriver_holds(Pending::StartedOnly).await;
    }
}
