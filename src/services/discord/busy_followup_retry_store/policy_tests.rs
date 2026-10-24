use super::RetryClass::{Parked, Ready, Unavailable};
use super::UnavailableReason::{
    IncompleteRecord, IncompleteView, InvalidNotice, MalformedRecord, ReadFailed, RootUnavailable,
};
use super::*;
use crate::services::discord::busy_followup_retry_store as store;

const MINUTE_MS: u64 = 60_000;
const UNREAD: RetryClass = Unavailable(ReadFailed);

/// A memory or disk queue layer as the injection check saw it; `None` when left unchecked.
type Layer = Option<LayerSnapshot<'static>>;

fn layer(classes: &'static [RetryClass]) -> Layer {
    Some(LayerSnapshot::Classes(classes))
}

fn layers(memory: Layer, disk: Layer) -> bool {
    BacklogLayers { memory, disk }.admits_later_input()
}

fn with_root(test: impl FnOnce(&Path)) {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let root = tempfile::tempdir().expect("runtime root");
    let _guard = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        root.path(),
    );
    let retries = runtime_store::discord_busy_followup_retries_root().expect("retry root");
    test(&retries);
    store::set_clock_for_tests(None);
}

fn budget(count: u32, active_ms: u64, capped: bool) -> RetryBudget {
    RetryBudget {
        notice_message_id: 700,
        busy_retry_count: count,
        first_busy_retry_at_ms: 1_000,
        draft_hold: false,
        active_retry_ms: active_ms,
        capped,
        notice_bound: true,
        draft_session: None,
    }
}

fn unreadable() -> SourceRead {
    SourceRead::Unreadable(ReadRefusal::new(ReadFailed, "test"))
}

/// Classifies reads through a complete view: ids 1..=n, the first one primary.
fn class_of(reads: &[&SourceRead]) -> RetryClass {
    let reads: Vec<(u64, SourceRead)> = (1..)
        .zip(reads.iter().map(|read| (*read).clone()))
        .collect();
    let sources: Vec<MessageId> = reads
        .iter()
        .skip(1)
        .map(|(id, _)| MessageId::new(*id))
        .collect();
    CheckedInputView::from_reads(&ProviderKind::Claude, 1, 1, &sources, reads).classify()
}

fn class_of_bytes(bytes: &[u8]) -> RetryClass {
    class_of(&[&decode_checked(bytes).map_or_else(SourceRead::Unreadable, SourceRead::Budget)])
}

#[cfg(unix)]
fn capped_draft(channel: u64, session: &str) -> CheckedInputView {
    for _ in 0..MAX_BUSY_RETRY_COUNT {
        store::record_attempt(
            &ProviderKind::Claude,
            (channel, 101, 700),
            &[],
            std::time::Duration::ZERO,
            None,
        )
        .unwrap();
    }
    store::record_attempt(
        &ProviderKind::Claude,
        (channel, 101, 700),
        &[],
        std::time::Duration::ZERO,
        Some(session),
    )
    .unwrap();
    CheckedInputView::read(&ProviderKind::Claude, channel, 101, &[])
}

#[cfg(unix)]
fn bind_idle(session: &str, path: &Path) {
    use crate::services::agent_protocol::RuntimeHandoffKind;
    crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
        session,
        crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            output_path: path.display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: None,
            last_offset: 0,
            relay_last_offset: None,
        },
    );
}

#[cfg(unix)]
async fn rewatch(
    channel: u64,
    session: &str,
) -> std::sync::Arc<crate::services::discord::SharedData> {
    use crate::services::claude_tui::composer_lock::{DraftGuard, guard_draft};
    guard_draft(session, DraftGuard::DraftRestored);
    let live = std::collections::HashMap::from([(
        serenity::all::ChannelId::new(channel),
        std::collections::HashSet::from([101]),
    )]);
    let shared = crate::services::discord::make_shared_data_for_tests();
    let mut state = crate::services::discord::InflightTurnState::new(
        ProviderKind::Claude,
        channel,
        None,
        1,
        101,
        700,
        "held source".into(),
        None,
        Some(session.into()),
        None,
        None,
        0,
    );
    state.followup_merge_consecutive = false;
    assert!(
        store::requeue_inflight_for_followup_retry(
            &shared,
            &ProviderKind::Claude,
            serenity::all::ChannelId::new(channel),
            &state
        )
        .await
        .enqueued
    );
    crate::services::discord::turn_bridge::followup_requeue::rewatch_draft_holds(
        &shared,
        &ProviderKind::Claude,
        &live,
    );
    shared
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn idle_capped_head_release_rereads_stale_view_and_repeated_hold_does_not_block_forever() {
    use crate::services::claude_tui::busy_inject::StashChannelGuard;
    use crate::services::claude_tui::host_input::guarded_submission::idle_stash::tests::{
        events, renderer, view,
    };
    use crate::services::discord::turn_bridge::followup_requeue::draft_release::idle_release_pending;
    use crate::services::tui_input::submission_tmux::guarded::transport_tests::Fixture;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::after_shared_test_env_lock();
    let channel = 6_899_981;
    let session = "policy-idle-release";
    let _listed = StashChannelGuard::new(channel);
    let pane = renderer(&f, session, "normal");
    bind_idle(session, &pane.join("transcript"));
    let stale = capped_draft(channel, session);
    assert_eq!(
        stale.classify(),
        Unavailable(UnavailableReason::IdleDraftRelease),
        "a preparation hold is unavailable even before observer registration"
    );
    let _shared = rewatch(channel, session).await;
    let held = Unavailable(UnavailableReason::IdleDraftRelease);
    assert_eq!(stale.classify(), held);
    crate::services::claude_tui::composer_lock::with_composer_mutation_lock(session, || {
        assert_eq!(
            stale.classify(),
            held,
            "a simultaneous watch capture must not let a later input overtake"
        );
    });
    assert_eq!(auto_pick(&[held, Ready]), None);
    assert!(!layers(
        layer(&[Unavailable(UnavailableReason::IdleDraftRelease)]),
        layer(&[])
    ));
    for _ in 0..8 {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        if idle_release_pending(channel, session) == Some(false) {
            break;
        }
    }
    assert_eq!(idle_release_pending(channel, session), Some(false));
    assert_eq!(
        stale.classify(),
        Ready,
        "a released head must not be skipped using its old capped snapshot"
    );
    let reheld = capped_draft(channel, session);
    assert_eq!(
        reheld.classify(),
        Unavailable(UnavailableReason::IdleDraftRelease),
        "an unchanged unsupported composer grants no new kickoff"
    );
    assert_eq!(auto_pick(&[reheld.classify(), Ready]), None);
    assert!(
        events(&pane).is_empty(),
        "classification and the watcher grant no submission authority"
    );
    std::fs::write(pane.join("tampered"), view("", false)).unwrap();
    f.tmux(&["send-keys", "-t", session, "C-r"]);
    for _ in 0..8 {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        if idle_release_pending(channel, session).is_none() {
            break;
        }
    }
    assert_eq!(
        CheckedInputView::read(&ProviderKind::Claude, channel, 101, &[]).classify(),
        Ready
    );
    assert_eq!(idle_release_pending(channel, session), None);
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn idle_order_boundary_preserves_general_legacy_busy_and_non_target_caps() {
    use crate::services::claude_tui::busy_inject::StashChannelGuard;
    use crate::services::claude_tui::host_input::guarded_submission::idle_stash::tests::{
        events, renderer,
    };
    use crate::services::tui_input::submission_tmux::guarded::transport_tests::Fixture;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::after_shared_test_env_lock();
    let channel = 6_899_982;
    let session = "policy-idle-controls";
    let _listed = StashChannelGuard::new(channel);
    let pane = renderer(&f, session, "normal");
    bind_idle(session, &pane.join("transcript"));
    let saved = capped_draft(channel, session);
    let _shared = rewatch(channel, session).await;
    assert_eq!(
        saved.classify(),
        Unavailable(UnavailableReason::IdleDraftRelease)
    );
    let classify = |provider: &ProviderKind, channel, budget| {
        CheckedInputView::from_reads(
            provider,
            channel,
            101,
            &[],
            vec![(101, SourceRead::Budget(budget))],
        )
        .classify()
    };
    let held = store::load(&ProviderKind::Claude, channel, 101).unwrap();
    for (name, provider, id, budget, expected) in [
        (
            "ordinary cap",
            ProviderKind::Claude,
            channel,
            RetryBudget {
                draft_hold: false,
                ..held.clone()
            },
            Parked,
        ),
        (
            "legacy without pane",
            ProviderKind::Claude,
            channel,
            RetryBudget {
                draft_session: None,
                ..held.clone()
            },
            Unavailable(UnavailableReason::IdleDraftRelease),
        ),
        (
            "other provider",
            ProviderKind::Codex,
            channel,
            held.clone(),
            Parked,
        ),
        (
            "unlisted channel",
            ProviderKind::Claude,
            channel + 1,
            held.clone(),
            Unavailable(UnavailableReason::IdleDraftRelease),
        ),
    ] {
        let class = classify(&provider, id, budget);
        assert_eq!(class, expected, "{name}");
        assert_eq!(
            auto_pick(&[class, Ready]),
            (class == Parked).then_some(1),
            "{name}"
        );
    }
    assert_eq!(
        classify(
            &ProviderKind::Claude,
            channel,
            RetryBudget {
                capped: false,
                busy_retry_count: 1,
                ..held.clone()
            }
        ),
        Unavailable(UnavailableReason::IdleDraftRelease)
    );
    std::fs::write(
        pane.join("transcript"),
        "{\"type\":\"user\",\"message\":{\"content\":\"running turn\"}}\n",
    )
    .unwrap();
    assert_eq!(
        saved.classify(),
        Unavailable(UnavailableReason::IdleDraftRelease),
        "a busy pane does not release a protected source"
    );
    crate::services::claude_tui::composer_lock::with_composer_mutation_lock(session, || {
        assert_eq!(
            saved.classify(),
            Unavailable(UnavailableReason::IdleDraftRelease),
            "lock contention does not release a protected source"
        );
    });
    assert_eq!(auto_pick(&[saved.classify(), Ready]), None);
    assert!(events(&pane).is_empty());
}

#[test]
fn queue_wait_costs_nothing_and_only_real_attempts_reach_the_cap() {
    let t0 = 10_000_000;
    let first = RetryBudget::charge(None, 700, 45_000, t0, None);
    let after_wait =
        RetryBudget::charge(Some(first), 700, 45_000, t0 + 6 * MINUTE_MS + 45_000, None);
    assert_eq!(
        (
            after_wait.busy_retry_count,
            after_wait.active_retry_ms,
            after_wait.capped
        ),
        (2, 90_000, false),
        "six minutes queued between two 45 s failures must not cap the input"
    );
    assert_eq!(after_wait.first_busy_retry_at_ms, t0);
    assert_eq!(class_of(&[&SourceRead::Budget(after_wait)]), Ready);

    let mut slow = None;
    for (attempt, capped) in [(1, false), (2, false), (3, false), (4, true)] {
        let charged = RetryBudget::charge(slow, 700, 80_000, t0 + attempt * MINUTE_MS, None);
        assert_eq!(
            (charged.active_retry_ms, charged.capped),
            (attempt * 80_000, capped)
        );
        slow = Some(charged);
    }

    let edge = RetryBudget::charge(None, 700, 299_999, t0, None);
    assert!(
        !edge.capped,
        "one millisecond short of the cost cap stays ready"
    );
    assert!(RetryBudget::charge(Some(edge), 700, 1, t0, None).capped);

    let mut counted = None;
    for count in 1..=MAX_BUSY_RETRY_COUNT {
        let charged = RetryBudget::charge(counted, 700, 1, t0, None);
        assert_eq!(charged.capped, count == MAX_BUSY_RETRY_COUNT);
        counted = Some(charged);
    }
    let five = (0..5).fold(None, |prior, _| {
        Some(RetryBudget::charge(prior, 700, 1, t0, None))
    });
    assert!(
        !five.as_ref().unwrap().capped,
        "five failures stay runnable"
    );
    assert!(
        RetryBudget::charge(five, 700, 1, t0, None).capped,
        "the sixth failure caps"
    );
}

#[test]
fn a_latched_cap_survives_time_and_later_charges() {
    let t0 = 10_000_000;
    let mut latched = None;
    for attempt in 0..4 {
        latched = Some(RetryBudget::charge(
            latched,
            700,
            80_000,
            t0 + attempt,
            None,
        ));
    }
    let latched = latched.unwrap();
    assert!(latched.capped);
    let bytes = serde_json::to_vec(&latched).unwrap();
    store::set_clock_for_tests(Some(t0 + 25 * 60 * MINUTE_MS));
    assert_eq!(class_of_bytes(&bytes), Parked);
    let flag_only = budget(1, 0, true);
    assert!(
        flag_only.exhausted(),
        "the latch alone keeps the input parked"
    );
    assert!(RetryBudget::charge(Some(flag_only), 700, 0, t0, None).capped);
    store::set_clock_for_tests(None);
}

#[test]
fn draft_release_clears_count_cost_and_latch_but_keeps_the_notice() {
    let held = RetryBudget {
        draft_hold: true,
        notice_bound: false,
        ..budget(MAX_BUSY_RETRY_COUNT, 320_000, true)
    };
    assert_eq!(
        held.released_from_draft(),
        RetryBudget {
            notice_message_id: 700,
            busy_retry_count: 0,
            first_busy_retry_at_ms: 0,
            draft_hold: false,
            active_retry_ms: 0,
            capped: false,
            notice_bound: false,
            draft_session: None,
        }
    );
}

#[test]
fn checked_restore_keeps_legacy_meaning_and_refuses_what_it_cannot_read() {
    let restored = |raw: &str| decode_checked(raw.as_bytes());
    let legacy_unbound =
        br#"{"notice_message_id":700,"busy_retry_count":0,"first_busy_retry_at_ms":0}"#;
    assert_eq!(
        decode_checked(legacy_unbound).unwrap(),
        RetryBudget {
            first_busy_retry_at_ms: 0,
            ..budget(0, 0, false)
        }
    );
    store::set_clock_for_tests(Some(1_000 + 6 * MINUTE_MS));
    let legacy_one =
        r#"{"notice_message_id":700,"busy_retry_count":1,"first_busy_retry_at_ms":1000}"#;
    assert_eq!(restored(legacy_one).unwrap(), budget(1, 0, false));
    assert_eq!(
        class_of_bytes(legacy_one.as_bytes()),
        Ready,
        "legacy age is not a cap"
    );
    let legacy_six =
        r#"{"notice_message_id":700,"busy_retry_count":6,"first_busy_retry_at_ms":1000}"#;
    assert_eq!(restored(legacy_six).unwrap().busy_retry_count, 6);
    assert_eq!(class_of_bytes(legacy_six.as_bytes()), Parked);
    let legacy_draft = r#"{"notice_message_id":700,"busy_retry_count":2,"first_busy_retry_at_ms":1000,"draft_hold":true}"#;
    assert!(restored(legacy_draft).unwrap().draft_hold);
    let latched = r#"{"notice_message_id":700,"busy_retry_count":2,"first_busy_retry_at_ms":1000,"active_retry_ms":0,"capped":true,"notice_bound":false}"#;
    assert_eq!(class_of_bytes(latched.as_bytes()), Parked);
    let spent = r#"{"notice_message_id":700,"busy_retry_count":2,"first_busy_retry_at_ms":1000,"active_retry_ms":300000,"capped":false,"notice_bound":true}"#;
    assert_eq!(class_of_bytes(spent.as_bytes()), Parked);
    store::set_clock_for_tests(None);

    for (case, raw, reason) in [
        (
            "truncated",
            r#"{"notice_message_id":700,"busy_retry_count":1,"first_bu"#,
            MalformedRecord,
        ),
        ("empty", "", MalformedRecord),
        ("not an object", "[700, 1, 1000]", MalformedRecord),
        (
            "missing count",
            r#"{"notice_message_id":700,"first_busy_retry_at_ms":1000}"#,
            MalformedRecord,
        ),
        (
            "missing first",
            r#"{"notice_message_id":700,"busy_retry_count":1}"#,
            MalformedRecord,
        ),
        (
            "unknown field",
            r#"{"notice_message_id":700,"busy_retry_count":1,"first_busy_retry_at_ms":1000,"retry_epoch":2}"#,
            MalformedRecord,
        ),
        (
            "wrong type",
            r#"{"notice_message_id":700,"busy_retry_count":"1","first_busy_retry_at_ms":1000}"#,
            MalformedRecord,
        ),
        (
            "zero notice",
            r#"{"notice_message_id":0,"busy_retry_count":1,"first_busy_retry_at_ms":1000}"#,
            InvalidNotice,
        ),
    ] {
        assert_eq!(
            restored(raw).map_err(|refusal| refusal.reason),
            Err(reason),
            "{case}"
        );
        assert_eq!(
            class_of_bytes(raw.as_bytes()),
            Unavailable(reason),
            "{case}"
        );
    }
}

#[test]
fn latch_fields_restore_only_when_all_or_none_are_written() {
    let legacy = Ok(budget(2, 0, false));
    let full = Ok(RetryBudget {
        notice_bound: false,
        ..budget(2, 0, true)
    });
    let partial = Err(IncompleteRecord);
    // Presence of (active_retry_ms, capped, notice_bound), written by hand.
    let rows: [(&str, Result<RetryBudget, UnavailableReason>); 10] = [
        (
            r#"{"notice_message_id":700,"busy_retry_count":2,"first_busy_retry_at_ms":1000}"#,
            legacy,
        ),
        (
            r#"{"notice_message_id":700,"busy_retry_count":2,"first_busy_retry_at_ms":1000,"active_retry_ms":0}"#,
            partial.clone(),
        ),
        (
            r#"{"notice_message_id":700,"busy_retry_count":2,"first_busy_retry_at_ms":1000,"capped":true}"#,
            partial.clone(),
        ),
        (
            r#"{"notice_message_id":700,"busy_retry_count":2,"first_busy_retry_at_ms":1000,"notice_bound":false}"#,
            partial.clone(),
        ),
        (
            r#"{"notice_message_id":700,"busy_retry_count":2,"first_busy_retry_at_ms":1000,"active_retry_ms":0,"capped":true}"#,
            partial.clone(),
        ),
        (
            r#"{"notice_message_id":700,"busy_retry_count":2,"first_busy_retry_at_ms":1000,"active_retry_ms":0,"notice_bound":false}"#,
            partial.clone(),
        ),
        (
            r#"{"notice_message_id":700,"busy_retry_count":2,"first_busy_retry_at_ms":1000,"capped":true,"notice_bound":false}"#,
            partial.clone(),
        ),
        (
            r#"{"notice_message_id":700,"busy_retry_count":2,"first_busy_retry_at_ms":1000,"active_retry_ms":0,"capped":true,"notice_bound":false}"#,
            full,
        ),
        (
            r#"{"notice_message_id":700,"busy_retry_count":2,"first_busy_retry_at_ms":1000,"active_retry_ms":0,"capped":null,"notice_bound":false}"#,
            partial.clone(),
        ),
        (
            r#"{"notice_message_id":700,"busy_retry_count":2,"first_busy_retry_at_ms":1000,"active_retry_ms":null,"capped":null,"notice_bound":null}"#,
            partial.clone(),
        ),
    ];
    for (index, (raw, expected)) in rows.into_iter().enumerate() {
        let decoded = decode_checked(raw.as_bytes());
        assert_eq!(
            decoded.clone().map_err(|refusal| refusal.reason),
            expected,
            "row {index}"
        );
        if let Err(refusal) = decoded {
            assert!(
                refusal.detail.contains("latch fields"),
                "row {index}: {}",
                refusal.detail
            );
        }
    }
    assert_eq!(
        class_of_bytes(rows_partial_capped().as_bytes()),
        Unavailable(IncompleteRecord)
    );
}

/// The latched record of the restore fixtures with only its `capped` field dropped.
fn rows_partial_capped() -> &'static str {
    r#"{"notice_message_id":700,"busy_retry_count":2,"first_busy_retry_at_ms":1000,"active_retry_ms":0,"notice_bound":false}"#
}

#[test]
fn source_precedence_and_queue_selection_follow_the_written_tables() {
    let ready = SourceRead::Budget(budget(1, 45_000, false));
    let capped = SourceRead::Budget(budget(2, 0, true));
    let (absent, unreadable) = (SourceRead::Absent, unreadable());
    let inputs: [(&[&SourceRead], RetryClass); 11] = [
        (&[&absent], Ready),
        (&[&ready], Ready),
        (&[&capped], Parked),
        (&[&unreadable], UNREAD),
        (&[&absent, &capped], Parked),
        (&[&ready, &capped], Parked),
        (&[&capped, &ready, &absent], Parked),
        (&[&capped, &unreadable], UNREAD),
        (&[&unreadable, &capped], UNREAD),
        (&[&ready, &unreadable], UNREAD),
        (&[&absent, &absent], Ready),
    ];
    for (index, (reads, expected)) in inputs.iter().enumerate() {
        assert_eq!(class_of(reads), *expected, "input row {index}");
    }

    let picks: [(&[RetryClass], Option<usize>); 7] = [
        (&[], None),
        (&[Ready, Ready], Some(0)),
        (&[Parked, Ready], Some(1)),
        (&[Parked, Parked], None),
        (&[UNREAD, Ready], None),
        (&[Parked, UNREAD, Ready], None),
        (&[Parked, Ready, UNREAD], Some(1)),
    ];
    for (classes, expected) in picks {
        assert_eq!(auto_pick(classes), expected, "{classes:?}");
    }

    let unread_layer = Some(LayerSnapshot::Unreadable);
    let table: [(Layer, Layer, bool); 9] = [
        (layer(&[]), layer(&[]), true),
        (layer(&[Parked]), layer(&[Parked]), true),
        (layer(&[Parked]), layer(&[]), true),
        (layer(&[Parked, Ready]), layer(&[Parked]), false),
        (layer(&[Parked]), layer(&[Ready]), false),
        (layer(&[]), layer(&[UNREAD]), false),
        (layer(&[UNREAD]), layer(&[Parked]), false),
        (unread_layer, layer(&[]), false),
        (layer(&[Parked]), unread_layer, false),
    ];
    for (memory, disk, admits) in table {
        assert_eq!(
            layers(memory, disk),
            admits,
            "memory={memory:?} disk={disk:?}"
        );
    }
}

#[test]
fn an_unreadable_source_holds_the_queue_until_it_reads_as_parked() {
    with_root(|root| {
        let provider = ProviderKind::Claude;
        let channel = 68_970;
        let path = |id: u64| store::input_file_path_in_root(root, &provider, channel, id);
        let (primary, capped_source, broken_source, later) = (68_971, 68_972, 68_973, 68_974);
        fs::create_dir_all(path(primary).parent().unwrap()).unwrap();
        fs::write(
            path(primary),
            br#"{"notice_message_id":700,"busy_retry_count":0,"first_busy_retry_at_ms":0}"#,
        )
        .unwrap();
        fs::write(
            path(capped_source),
            serde_json::to_vec(&budget(2, 0, true)).unwrap(),
        )
        .unwrap();
        fs::create_dir(path(broken_source)).unwrap();
        let sources = [capped_source, broken_source, primary].map(MessageId::new);
        let head = CheckedInputView::read(&provider, channel, primary, &sources);
        let tail = CheckedInputView::read(&provider, channel, later, &[]);
        let ids: Vec<u64> = head.reads().iter().map(|(id, _)| *id).collect();
        assert_eq!(
            ids,
            [primary, capped_source, broken_source],
            "each id is read once"
        );
        assert_eq!(
            head.reads()[1].1,
            SourceRead::Budget(budget(2, 0, true)),
            "the capped id is named"
        );
        let blocked = [head.classify(), tail.classify()];
        assert_eq!(blocked, [UNREAD, Ready]);
        assert_eq!(
            auto_pick(&blocked),
            None,
            "B must not overtake an unconfirmed A"
        );
        assert!(!layers(layer(&[UNREAD]), layer(&[])));

        fs::remove_dir(path(broken_source)).unwrap();
        let healed = [head.reread().classify(), tail.reread().classify()];
        assert_eq!(healed, [Parked, Ready]);
        assert_eq!(auto_pick(&healed), Some(1));
        assert!(layers(layer(&[Parked]), layer(&[])));
    });
}

/// The writer keeps every latch field, waiting adds no cost, and a rolled-back binary that knows
/// only the legacy fields still reads what the writer stored.
#[test]
fn writer_stores_the_latch_and_a_rolled_back_reader_still_reads_it() {
    #[derive(Debug, PartialEq, serde::Deserialize)]
    struct RolledBack {
        notice_message_id: u64,
        busy_retry_count: u32,
        first_busy_retry_at_ms: u64,
        #[serde(default)]
        draft_hold: bool,
    }
    with_root(|root| {
        let provider = ProviderKind::Claude;
        let channel = 68_975;
        let path = store::input_file_path_in_root(root, &provider, channel, 68_976);
        store::bind_notice_if_absent(&provider, channel, 68_976, 700).unwrap();
        let bound = decode_checked(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            bound,
            RetryBudget {
                first_busy_retry_at_ms: 0,
                ..budget(0, 0, false)
            }
        );
        store::set_clock_for_tests(Some(5_000));
        store::record_busy_retry(&provider, channel, 68_976, 701).unwrap();
        store::set_clock_for_tests(Some(5_000 + 6 * MINUTE_MS));
        let ids = (channel, 68_976, 701);
        let held =
            store::record_attempt(&provider, ids, &[], std::time::Duration::ZERO, Some("pane"))
                .unwrap();
        assert!(
            !held.capped,
            "six minutes of waiting must not cap the input"
        );
        let bytes = fs::read(&path).unwrap();
        let stored = decode_checked(&bytes).unwrap();
        assert_eq!(stored, held.state);
        assert_eq!(
            (stored.busy_retry_count, stored.first_busy_retry_at_ms),
            (1, 5_000)
        );
        assert_eq!(stored.draft_session.as_deref(), Some("pane"));
        let rolled_back: RolledBack = serde_json::from_slice(&bytes).unwrap();
        let expected = RolledBack {
            notice_message_id: 700,
            busy_retry_count: 1,
            first_busy_retry_at_ms: 5_000,
            draft_hold: true,
        };
        assert_eq!(rolled_back, expected);
    });
}

#[test]
fn each_refusal_keeps_its_reason_and_absence_stays_ready() {
    with_root(|root| {
        let provider = ProviderKind::Claude;
        let channel = 68_980;
        let path = |id: u64| store::input_file_path_in_root(root, &provider, channel, id);
        fs::create_dir_all(path(1).parent().unwrap()).unwrap();
        fs::create_dir(path(2)).unwrap();
        fs::write(
            path(3),
            r#"{"notice_message_id":700,"busy_retry_count":1,"first_bu"#,
        )
        .unwrap();
        fs::write(
            path(4),
            r#"{"notice_message_id":0,"busy_retry_count":1,"first_busy_retry_at_ms":1000}"#,
        )
        .unwrap();
        fs::write(path(5), rows_partial_capped()).unwrap();
        fs::write(path(6), serde_json::to_vec(&budget(2, 0, true)).unwrap()).unwrap();
        let class = |primary: u64, sources: &[u64]| {
            let sources: Vec<MessageId> = sources.iter().copied().map(MessageId::new).collect();
            CheckedInputView::read(&provider, channel, primary, &sources).classify()
        };
        assert_eq!(class(1, &[]), Ready, "a confirmed absence is not a refusal");
        assert_eq!(class(2, &[]), Unavailable(ReadFailed));
        assert_eq!(class(3, &[]), Unavailable(MalformedRecord));
        assert_eq!(class(4, &[]), Unavailable(InvalidNotice));
        assert_eq!(class(5, &[]), Unavailable(IncompleteRecord));
        assert_eq!(
            class(6, &[2]),
            Unavailable(ReadFailed),
            "a refusal outranks a cap"
        );
        assert_eq!(class(6, &[1]), Parked);
        let rootless = CheckedInputView::read_in(None, &provider, channel, 1, &[MessageId::new(6)]);
        assert_eq!(rootless.classify(), Unavailable(RootUnavailable));
        assert_eq!(
            rootless.reads().len(),
            2,
            "every identity id carries the refusal"
        );
    });
}

#[test]
fn classification_refuses_views_and_layers_it_cannot_vouch_for() {
    let provider = ProviderKind::Claude;
    let capped = SourceRead::Budget(budget(2, 0, true));
    let view = |primary: u64, sources: &[u64], reads: Vec<(u64, SourceRead)>| {
        let sources: Vec<MessageId> = sources.iter().copied().map(MessageId::new).collect();
        CheckedInputView::from_reads(&provider, 7, primary, &sources, reads).classify()
    };
    let refused = Unavailable(IncompleteView);
    assert_eq!(view(1, &[], vec![]), refused, "no read at all");
    assert_eq!(
        view(1, &[2], vec![(1, SourceRead::Absent)]),
        refused,
        "a source left unread"
    );
    assert_eq!(
        view(
            1,
            &[],
            vec![(1, SourceRead::Absent), (1, SourceRead::Absent)]
        ),
        refused,
        "an id read twice"
    );
    assert_eq!(
        view(1, &[], vec![(1, SourceRead::Absent), (9, capped.clone())]),
        refused,
        "a read outside the identity"
    );
    assert_eq!(
        view(0, &[], vec![(0, SourceRead::Absent)]),
        refused,
        "no primary identity"
    );
    assert_eq!(
        view(1, &[], vec![(1, SourceRead::Absent)]),
        Ready,
        "a confirmed absent primary"
    );
    assert_eq!(
        view(1, &[2, 1], vec![(2, capped), (1, SourceRead::Absent)]),
        Parked,
        "read order is free"
    );

    assert!(
        !BacklogLayers::default().admits_later_input(),
        "no layer checked"
    );
    assert!(!layers(layer(&[]), None), "disk left unchecked");
    assert!(!layers(None, layer(&[])), "memory left unchecked");
    assert!(
        layers(layer(&[]), layer(&[])),
        "both queues confirmed empty"
    );
}

#[test]
fn a2_preparation_wait_never_spends_busy_retry_budget() {
    let mut held = None;
    for _ in 0..12 {
        held = Some(RetryBudget::charge(
            held,
            700,
            90_000,
            1_000,
            Some("held-pane"),
        ));
    }
    let held = held.unwrap();
    assert_eq!(
        (held.busy_retry_count, held.active_retry_ms, held.capped),
        (0, 0, false),
        "preparation waiting is not a busy attempt"
    );
    assert_ne!(
        classify_reads([&SourceRead::Budget(held)], true),
        Ready,
        "a held source must not start an AI turn"
    );
}
