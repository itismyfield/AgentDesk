use super::*;
use crate::db::relay_dead_letter as dl;

const SESSION: &str = "adk-claude-5551";
const FULL: &str = "0123456789ABCDEFGHIJ";

fn reason_for(start: usize, end: usize, generation: i64) -> String {
    format!(
        "terminal_no_delivery_owner denial=turn_nonce_mismatch terminal_kind=result \
         response_sent_offset={start} full_response_len={end} jsonl_start=100 jsonl_end=900 \
         current_offset=900 generation_mtime_ns={generation} tmux_session={SESSION} \
         provider=claude inflight_relay_owner=none frame_ack_outcome=TimedOut"
    )
}

fn row(id: i64, channel: &str, anchor: Option<&str>, body: &str, reason: String) -> ClaimedRow {
    ClaimedRow {
        id,
        channel_id: channel.into(),
        message_id: anchor.map(Into::into),
        content: body.into(),
        reason,
    }
}

/// The common shape: one channel, one stranded placeholder, one generation.
fn plain(id: i64, body: &str, start: usize, end: usize) -> ClaimedRow {
    row(id, "5551", Some("7001"), body, reason_for(start, end, 42))
}

fn bodies(plan: &RedeliveryPlan) -> Vec<&str> {
    plan.slices.iter().map(|s| s.body.as_str()).collect()
}

type ClaimedRow = dl::ClaimedDeadLetter;

#[test]
fn plan_emits_each_byte_once_and_supersedes_rows_that_add_none() {
    // Premise: row 3 must really extend row 2 and row 4 must really sit inside
    // it, or neither the trim nor the supersede below is exercised.
    assert!(10 < FULL.len(), "fixture rows must overlap");
    let plan = build_plan(vec![
        plain(2, &FULL[..10], 0, 10),
        plain(3, FULL, 0, FULL.len()),
        plain(4, &FULL[5..], 5, FULL.len()),
    ]);
    assert_eq!(
        bodies(&plan),
        vec!["0123456789", "ABCDEFGHIJ"],
        "a wider row may contribute only the bytes past the frontier"
    );
    assert_eq!(
        bodies(&plan).concat(),
        FULL,
        "within one claim batch the union of the posts is the recorded tail, once"
    );
    assert_eq!(
        plan.superseded,
        vec![4],
        "a row inside the frontier settles superseded, not delivered"
    );
    assert!(plan.declined.is_empty());
}

#[test]
fn plan_keeps_channels_anchors_and_generations_apart() {
    let plan = build_plan(vec![
        row(1, "5551", Some("7001"), "tail", reason_for(0, 4, 42)),
        row(2, "6662", Some("8002"), "tail", reason_for(0, 4, 42)),
        row(3, "5551", Some("7003"), "tail", reason_for(0, 4, 99)),
        // Row 4 differs from row 1 in the placeholder ONLY: same channel, same
        // session, same generation. That is two turns of one wrapper, the shape
        // a generation-keyed group merges and then trims to nothing.
        row(4, "5551", Some("7004"), "tail", reason_for(0, 4, 42)),
    ]);
    let mut seen: Vec<(i64, u64, u64)> = plan
        .slices
        .iter()
        .map(|s| (s.row_id, s.channel_id, s.anchor_message_id))
        .collect();
    seen.sort_unstable();
    assert_eq!(
        seen,
        vec![
            (1, 5551, 7001),
            (2, 6662, 8002),
            (3, 5551, 7003),
            (4, 5551, 7004)
        ],
        "another channel, placeholder or generation is another group, and each slice keeps its OWN channel and anchor"
    );
    assert!(plan.superseded.is_empty());
}

#[test]
fn plan_declines_rows_it_cannot_place_or_witness() {
    let plan = build_plan(vec![
        // A 9-byte span over a 4-byte body: the two are not one coordinate system.
        plain(1, "four", 0, 9),
        // No placeholder anchor ⇒ no witness that the body is still missing.
        row(2, "5551", None, "tail", reason_for(0, 4, 42)),
        row(3, "5551", Some("7001"), "tail", "no offsets here".into()),
        row(
            4,
            "not-a-channel",
            Some("7001"),
            "tail",
            reason_for(0, 4, 42),
        ),
    ]);
    assert!(plan.slices.is_empty(), "none of these rows may be posted");
    let mut declined = plan.declined.clone();
    declined.sort_unstable();
    assert_eq!(declined, vec![1, 2, 3, 4]);
}

#[test]
fn plan_trims_multibyte_bodies_on_a_character_boundary() {
    let wide = "가나다라";
    // Premise: the cut lands inside a character; that is the shape under test.
    assert!(!wide.is_char_boundary(2), "fixture must cut inside a char");
    let plan = build_plan(vec![
        plain(1, "abc", 0, 3),
        plain(2, wide, 1, 1 + wide.len()),
    ]);
    assert_eq!(
        bodies(&plan),
        vec!["abc", "나다라"],
        "the trim must round up to the next character, never split one"
    );
}

#[test]
fn the_claim_window_is_non_empty_under_the_delivered_content_witness() {
    // MAX_AGE_SECS is derived from the fingerprint window, so it cannot outlive
    // it. What is still free to break is MIN_AGE_SECS overrunning it.
    assert!(
        MIN_AGE_SECS < MAX_AGE_SECS,
        "the claim window must be non-empty"
    );
}

#[test]
fn watcher_reason_format_still_carries_every_key_this_module_parses() {
    let writer = include_str!("../../tmux_watcher/orphan_terminal_frame.rs");
    for key in [
        "response_sent_offset={response_sent_offset}",
        "full_response_len={full_response_len}",
        "generation_mtime_ns={generation_mtime_ns}",
        "tmux_session={tmux_session}",
        "provider={provider}",
    ] {
        assert!(
            writer.contains(key),
            "orphan_terminal_frame::reason dropped {key}; parse_span still reads it"
        );
    }
    let parsed = parse_span(&reason_for(7, 19, 42), 12).expect("round trip");
    assert_eq!(
        (parsed.start, parsed.end, parsed.generation_mtime_ns),
        (7, 19, 42)
    );
    assert_eq!(
        (parsed.tmux_session.as_str(), parsed.provider.as_str()),
        (SESSION, "claude")
    );
}

/// Deleting the bootstrap call, refusing before the spawn, or dropping the
/// sink's witnesses are all silent otherwise: every other test here enters at
/// `build_plan` or injects its own sink, so neither seam is ever executed. A
/// lexical adjacency guard, like the sibling sweep's at
/// `intake_delivery_sweep/tests.rs`; a matching token in a comment would satisfy
/// it, and runtime behaviour is not what it claims to cover.
#[test]
fn spawn_and_sink_wiring_is_pinned_against_silent_deletion() {
    let source = include_str!("../relay_dlq_redelivery.rs");
    let at = |needle: &str| {
        source
            .find(needle)
            .unwrap_or_else(|| panic!("{needle} is gone"))
    };
    assert_eq!(
        include_str!("../framework_setup.rs")
            .matches("spawn_relay_dlq_redelivery(shared_clone.clone())")
            .count(),
        1,
        "bootstrap wires exactly one redelivery sweep spawn per bot"
    );
    assert_eq!(
        source.matches("return false").count(),
        2,
        "the spawn refuses for exactly two reasons: no pool, and already running"
    );
    assert!(
        at("compare_exchange") < at("spawn_observed"),
        "the process latch is claimed before the task spawns"
    );
    let post = at("send_long_message_raw_with_reference");
    for witness in [
        "text_ends_with_streaming_footer",
        "recent_delivered_content_matches",
    ] {
        assert!(
            at(witness) < post,
            "{witness} must gate the POST, not follow it"
        );
    }
    // Only the unreadable-anchor path returns Deferred directly; the failed-POST
    // path is a tail expression. Turning this one into Declined is what retires a
    // body on a 429, and no behavioural test reaches it (see the follow-up issue).
    assert_eq!(
        source.matches("return SliceOutcome::Deferred").count(),
        1,
        "an unreadable witness must defer: a transient failure is not a verdict"
    );
}

#[derive(Default)]
struct CapturingSink {
    seen: std::sync::Mutex<Vec<RedeliverySlice>>,
}

impl SliceSink for CapturingSink {
    async fn deliver(&self, slice: &RedeliverySlice) -> SliceOutcome {
        self.seen.lock().expect("sink lock").push(slice.clone());
        SliceOutcome::Delivered
    }
}

/// Stands in for a Discord that could not be read or written this tick.
struct DeferringSink;

impl SliceSink for DeferringSink {
    async fn deliver(&self, _slice: &RedeliverySlice) -> SliceOutcome {
        SliceOutcome::Deferred
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sweep_returns_the_lost_body_to_its_channel_pg() {
    let pg_db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
        "agentdesk_relay_dlq_sweep",
        "relay dlq redelivery sweep",
    )
    .await;
    let pool = pg_db.connect_and_migrate().await;
    let record = |kind: &str, anchor: Option<&str>, body: &str| dl::RelayDeadLetterRecord {
        kind: kind.into(),
        channel_id: "5551".into(),
        author_id: Some("42".into()),
        message_id: anchor.map(Into::into),
        content: body.into(),
        reason: reason_for(0, body.len(), 42),
    };
    let terminal = dl::KIND_TERMINAL_NO_DELIVERY_OWNER;
    let mut ids = Vec::new();
    for (spec, age_secs) in [
        (record(terminal, Some("7001"), &FULL[..10]), 300),
        (record(terminal, Some("7001"), FULL), 300),
        (record(terminal, None, "no anchor"), 300),
        (record(dl::KIND_QUEUE_OVERFLOW, None, "an eviction"), 300),
        (record(terminal, Some("7001"), "too young"), 10),
        (record(terminal, Some("7001"), "too old"), 5_000),
    ] {
        let id = dl::insert(&pool, &spec).await.expect("insert");
        sqlx::query(
            "UPDATE relay_dead_letter
                SET created_at = NOW() - ($2::BIGINT * INTERVAL '1 second')
              WHERE id = $1",
        )
        .bind(id)
        .bind(age_secs as i64)
        .execute(&pool)
        .await
        .expect("age row");
        ids.push(id);
    }
    // Reads state and the settle stamp in one statement: "delivered +stamped".
    let state_of = |id: i64| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, String>(
                "SELECT redelivery_state
                        || CASE WHEN redelivered_at IS NULL THEN '' ELSE ' +stamped' END
                   FROM relay_dead_letter WHERE id = $1",
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .expect("read state")
        }
    };
    // Premise: six pending rows, or every tally below is vacuously zero.
    for id in &ids {
        assert_eq!(state_of(*id).await, "pending");
    }

    let sink = CapturingSink::default();
    let tally = sweep_once_with(&pool, &sink).await.expect("sweep");
    assert_eq!(
        tally,
        RedeliveryTally {
            delivered: 2,
            superseded: 0,
            declined: 1,
            deferred: 0
        }
    );
    let seen = sink.seen.lock().expect("sink lock").clone();
    assert_eq!(
        seen.iter().map(|s| s.body.as_str()).collect::<Vec<_>>(),
        vec!["0123456789", "ABCDEFGHIJ"],
        "the sweep hands the sink the merged tail, not the raw rows"
    );
    for slice in &seen {
        let got = (
            slice.channel_id,
            slice.anchor_message_id,
            slice.tmux_session.as_str(),
        );
        assert_eq!(
            got,
            (5551, 7001, SESSION),
            "each POST keeps its row's routing"
        );
    }
    for id in [ids[0], ids[1]] {
        let want = format!("{} +stamped", dl::REDELIVERY_DELIVERED);
        assert_eq!(
            state_of(id).await,
            want,
            "a posted row settles and is stamped"
        );
    }
    assert_eq!(
        state_of(ids[2]).await,
        format!("{} +stamped", dl::REDELIVERY_DECLINED),
        "no placeholder anchor ⇒ no witness ⇒ never posted"
    );
    for (id, why) in [
        (ids[3], "a non-terminal loss vector"),
        (ids[4], "a row younger than MIN_AGE_SECS"),
        (ids[5], "a row older than MAX_AGE_SECS"),
    ] {
        assert_eq!(state_of(id).await, "pending", "{why} is never claimed");
    }
    assert_eq!(
        sweep_once_with(&pool, &sink).await.expect("second sweep"),
        RedeliveryTally::default(),
        "a settled row is never redelivered a second time"
    );

    // A sink that could not read or write Discord this tick has said nothing
    // about the body, so the row must come back rather than retire unread.
    let deferred_id = dl::insert(&pool, &record(terminal, Some("7009"), "deferred tail"))
        .await
        .expect("insert");
    sqlx::query(
        "UPDATE relay_dead_letter
            SET created_at = NOW() - INTERVAL '300 seconds' WHERE id = $1",
    )
    .bind(deferred_id)
    .execute(&pool)
    .await
    .expect("age row");
    assert_eq!(
        sweep_once_with(&pool, &DeferringSink)
            .await
            .expect("deferring sweep"),
        RedeliveryTally {
            deferred: 1,
            ..RedeliveryTally::default()
        }
    );
    assert_eq!(
        state_of(deferred_id).await,
        "pending +stamped",
        "a deferred row returns to pending, stamped with the attempt"
    );
    let retry = CapturingSink::default();
    assert_eq!(
        sweep_once_with(&pool, &retry)
            .await
            .expect("retry sweep")
            .delivered,
        1,
        "a transient failure defers the body, it does not discard it"
    );
    assert_eq!(
        state_of(deferred_id).await,
        format!("{} +stamped", dl::REDELIVERY_DELIVERED)
    );

    pool.close().await;
    pg_db.drop().await;
}
