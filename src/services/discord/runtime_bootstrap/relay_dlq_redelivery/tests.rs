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
    assert!(plan.unaddressable.is_empty());
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
fn plan_refuses_rows_it_cannot_address() {
    let plan = build_plan(vec![
        // A 9-byte span over a 4-byte body: the two are not one coordinate
        // system, so no byte range of this row can be named at all.
        plain(1, "four", 0, 9),
        // No placeholder anchor ⇒ nothing to reply to.
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
    let mut unaddressable = plan.unaddressable.clone();
    unaddressable.sort_unstable();
    assert_eq!(unaddressable, vec![1, 2, 3, 4]);
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
