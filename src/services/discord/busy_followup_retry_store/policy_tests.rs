use super::RetryClass::{Parked, Ready, Unavailable};
use super::*;
use crate::services::discord::busy_followup_retry_store as store;

const MINUTE_MS: u64 = 60_000;

/// A memory or disk queue snapshot; `None` when it could not be read.
type Layer = Option<&'static [RetryClass]>;

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
    }
}

fn class_of_bytes(bytes: &[u8]) -> RetryClass {
    let read = decode_checked(bytes).map_or(SourceRead::Unreadable, SourceRead::Budget);
    classify_input([&read])
}

#[test]
fn queue_wait_costs_nothing_and_only_real_attempts_reach_the_cap() {
    let t0 = 10_000_000;
    let first = RetryBudget::charge(None, 700, 45_000, t0, false);
    let after_wait =
        RetryBudget::charge(Some(first), 700, 45_000, t0 + 6 * MINUTE_MS + 45_000, false);
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
    assert_eq!(classify_input([&SourceRead::Budget(after_wait)]), Ready);

    let mut slow = None;
    for (attempt, capped) in [(1, false), (2, false), (3, false), (4, true)] {
        let charged = RetryBudget::charge(slow, 700, 80_000, t0 + attempt * MINUTE_MS, false);
        assert_eq!(
            (charged.active_retry_ms, charged.capped),
            (attempt * 80_000, capped)
        );
        slow = Some(charged);
    }

    let edge = RetryBudget::charge(None, 700, 299_999, t0, false);
    assert!(
        !edge.capped,
        "one millisecond short of the cost cap stays ready"
    );
    assert!(RetryBudget::charge(Some(edge), 700, 1, t0, false).capped);

    let mut counted = None;
    for count in 1..=MAX_BUSY_RETRY_COUNT {
        let charged = RetryBudget::charge(counted, 700, 1, t0, false);
        assert_eq!(charged.capped, count == MAX_BUSY_RETRY_COUNT);
        counted = Some(charged);
    }
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
            false,
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
    assert!(RetryBudget::charge(Some(flag_only), 700, 0, t0, false).capped);
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

    for (case, raw) in [
        (
            "truncated",
            r#"{"notice_message_id":700,"busy_retry_count":1,"first_bu"#,
        ),
        ("empty", ""),
        (
            "missing count",
            r#"{"notice_message_id":700,"first_busy_retry_at_ms":1000}"#,
        ),
        (
            "missing first",
            r#"{"notice_message_id":700,"busy_retry_count":1}"#,
        ),
        (
            "unknown field",
            r#"{"notice_message_id":700,"busy_retry_count":1,"first_busy_retry_at_ms":1000,"retry_epoch":2}"#,
        ),
        (
            "wrong type",
            r#"{"notice_message_id":700,"busy_retry_count":"1","first_busy_retry_at_ms":1000}"#,
        ),
        (
            "zero notice",
            r#"{"notice_message_id":0,"busy_retry_count":1,"first_busy_retry_at_ms":1000}"#,
        ),
    ] {
        assert!(restored(raw).is_err(), "{case} must not restore");
        assert_eq!(class_of_bytes(raw.as_bytes()), Unavailable, "{case}");
    }
}

#[test]
fn source_precedence_and_queue_selection_follow_the_written_tables() {
    let ready = SourceRead::Budget(budget(1, 45_000, false));
    let capped = SourceRead::Budget(budget(2, 0, true));
    let (absent, unreadable) = (SourceRead::Absent, SourceRead::Unreadable);
    let inputs: [(&[&SourceRead], RetryClass); 11] = [
        (&[&absent], Ready),
        (&[&ready], Ready),
        (&[&capped], Parked),
        (&[&unreadable], Unavailable),
        (&[&absent, &capped], Parked),
        (&[&ready, &capped], Parked),
        (&[&capped, &ready, &absent], Parked),
        (&[&capped, &unreadable], Unavailable),
        (&[&unreadable, &capped], Unavailable),
        (&[&ready, &unreadable], Unavailable),
        (&[&absent, &absent], Ready),
    ];
    for (index, (reads, expected)) in inputs.iter().enumerate() {
        assert_eq!(
            classify_input(reads.iter().copied()),
            *expected,
            "input row {index}"
        );
    }

    let picks: [(&[RetryClass], Option<usize>); 7] = [
        (&[], None),
        (&[Ready, Ready], Some(0)),
        (&[Parked, Ready], Some(1)),
        (&[Parked, Parked], None),
        (&[Unavailable, Ready], None),
        (&[Parked, Unavailable, Ready], None),
        (&[Parked, Ready, Unavailable], Some(1)),
    ];
    for (classes, expected) in picks {
        assert_eq!(auto_pick(classes), expected, "{classes:?}");
    }

    let layers: [(Layer, Layer, bool); 9] = [
        (Some(&[]), Some(&[]), true),
        (Some(&[Parked]), Some(&[Parked]), true),
        (Some(&[Parked]), Some(&[]), true),
        (Some(&[Parked, Ready]), Some(&[Parked]), false),
        (Some(&[Parked]), Some(&[Ready]), false),
        (Some(&[]), Some(&[Unavailable]), false),
        (Some(&[Unavailable]), Some(&[Parked]), false),
        (None, Some(&[]), false),
        (Some(&[Parked]), None, false),
    ];
    for (memory, disk, admits) in layers {
        assert_eq!(
            admits_later_input(&[memory, disk]),
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
        let queue = |sources: &[MessageId]| {
            let head = read_sources(&provider, channel, primary, sources);
            let tail = read_sources(&provider, channel, later, &[]);
            [head, tail].map(|reads| classify_input(reads.iter().map(|(_, read)| read)))
        };

        let head = read_sources(&provider, channel, primary, &sources);
        let ids: Vec<u64> = head.iter().map(|(id, _)| *id).collect();
        assert_eq!(
            ids,
            [primary, capped_source, broken_source],
            "each id is read once"
        );
        assert_eq!(
            head[1].1,
            SourceRead::Budget(budget(2, 0, true)),
            "the capped id is named"
        );
        let blocked = queue(&sources);
        assert_eq!(blocked, [Unavailable, Ready]);
        assert_eq!(
            auto_pick(&blocked),
            None,
            "B must not overtake an unconfirmed A"
        );
        assert!(!admits_later_input(&[Some(&blocked[..1])]));

        fs::remove_dir(path(broken_source)).unwrap();
        let healed = queue(&sources);
        assert_eq!(healed, [Parked, Ready]);
        assert_eq!(auto_pick(&healed), Some(1));
        assert!(admits_later_input(&[Some(&healed[..1])]));
    });
}

#[test]
fn legacy_writer_bytes_and_reader_results_are_unchanged() {
    with_root(|root| {
        let provider = ProviderKind::Claude;
        let channel = 68_975;
        let path = store::input_file_path_in_root(root, &provider, channel, 68_976);
        store::bind_notice_if_absent(&provider, channel, 68_976, 700).unwrap();
        let bound = "{\n  \"notice_message_id\": 700,\n  \"busy_retry_count\": 0,\n  \"first_busy_retry_at_ms\": 0\n}";
        assert_eq!(fs::read_to_string(&path).unwrap(), bound);
        store::set_clock_for_tests(Some(5_000));
        let decision = store::record_busy_retry(&provider, channel, 68_976, 701).unwrap();
        let retried = "{\n  \"notice_message_id\": 700,\n  \"busy_retry_count\": 1,\n  \"first_busy_retry_at_ms\": 5000\n}";
        assert_eq!(fs::read_to_string(&path).unwrap(), retried);
        assert!(!decision.capped);
        store::set_clock_for_tests(Some(5_000 + 6 * MINUTE_MS));
        let held = store::record_draft_hold_retry(&provider, channel, 68_976, 701).unwrap();
        let held_bytes = "{\n  \"notice_message_id\": 700,\n  \"busy_retry_count\": 2,\n  \"first_busy_retry_at_ms\": 5000,\n  \"draft_hold\": true\n}";
        assert_eq!(fs::read_to_string(&path).unwrap(), held_bytes);
        assert!(
            held.capped,
            "the legacy reader keeps its wall-clock cap until the switch-over"
        );
        assert!(store::is_capped(&provider, channel, 68_976));

        let legacy = store::load(&provider, channel, 68_976).unwrap();
        let checked = decode_checked(held_bytes.as_bytes()).unwrap();
        assert_eq!(
            (
                checked.notice_message_id,
                checked.busy_retry_count,
                checked.first_busy_retry_at_ms,
                checked.draft_hold
            ),
            (
                legacy.notice_message_id,
                legacy.busy_retry_count,
                legacy.first_busy_retry_at_ms,
                legacy.draft_hold
            )
        );
        assert_eq!(
            (
                checked.active_retry_ms,
                checked.capped,
                checked.notice_bound
            ),
            (0, false, true)
        );

        let latched = serde_json::to_vec(&RetryBudget {
            draft_hold: true,
            ..budget(2, 90_000, true)
        })
        .unwrap();
        fs::write(&path, latched).unwrap();
        assert_eq!(
            store::load(&provider, channel, 68_976),
            Some(store::BusyFollowupRetryState {
                notice_message_id: 700,
                busy_retry_count: 2,
                first_busy_retry_at_ms: 1_000,
                draft_hold: true,
            }),
            "a rolled-back reader still reads the legacy fields of the new format"
        );
    });
}
