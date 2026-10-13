use super::*;
use crate::db::relay_dead_letter as dl;
use crate::services::discord::outbound::delivery_record;
use crate::services::provider::ProviderKind;
use poise::serenity_prelude::ChannelId;
use std::{fs, path::Path};

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

fn witness_plan() -> RedeliveryPlan {
    build_plan(vec![plain(1, "가나", 0, 6), plain(2, "가나다라", 0, 12)])
}

fn slice_has_witness(slice: &RedeliverySlice) -> bool {
    delivery_record::recent_delivered_content_matches_for_generation(
        &ProviderKind::from_str(&slice.provider).unwrap(),
        ChannelId::new(slice.channel_id),
        slice.generation_mtime_ns,
        &slice.body,
    )
}

fn write_witness(path: &Path, channel: u64, body: &str, generation: i64, delivered_at: u64) {
    let mut hash = blake3::Hasher::new();
    hash.update(&channel.to_le_bytes());
    hash.update(body.as_bytes());
    let record = delivery_record::DeliveryRecord {
        recent_delivered_contents: vec![delivery_record::DeliveredContentFingerprint {
            channel_id: channel,
            content_hash: hash.finalize().to_hex().to_string(),
            content_len: body.len() as u64,
            generation_mtime_ns: generation,
            delivered_at_epoch_ms: delivered_at,
        }],
        ..Default::default()
    };
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, serde_json::to_vec(&record).unwrap()).unwrap();
    assert_eq!(delivery_record::read_record_at(path), Some(record));
}

#[cfg(unix)]
#[test]
fn dlq_witness_uses_row_generation_after_current_generation_changes() {
    let root = tempfile::tempdir().unwrap();
    let _root_guard = crate::config::set_agentdesk_root_for_test(root.path());
    let marker = crate::services::tmux_common::session_temp_path(SESSION, "generation");
    fs::write(&marker, "fixture").unwrap();
    filetime::set_file_mtime(&marker, filetime::FileTime::from_unix_time(0, 42)).unwrap();
    assert_eq!(delivery_record::current_generation_mtime_ns(SESSION), 42);
    let plan = witness_plan();
    assert_eq!(bodies(&plan), vec!["가나", "다라"]);
    filetime::set_file_mtime(&marker, filetime::FileTime::from_unix_time(0, 99)).unwrap();
    assert_eq!(delivery_record::current_generation_mtime_ns(SESSION), 99);
    let path = delivery_record::delivery_record_path(&ProviderKind::Claude, 5551).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;

    for slice in &plan.slices {
        let mut results = Vec::new();
        for (generation, channel, body, expected) in [
            (42, 5551, slice.body.as_str(), true),
            (99, 5551, slice.body.as_str(), false),
            (42, 6662, slice.body.as_str(), false),
            (42, 5551, "다른", false),
            (42, 5551, "가나다라", false),
        ] {
            write_witness(&path, channel, body, generation, now);
            results.push((
                generation,
                channel,
                body.to_string(),
                slice_has_witness(slice),
                expected,
            ));
        }
        assert!(
            results
                .iter()
                .all(|(_, _, _, actual, expected)| actual == expected),
            "row={} witnesses={results:?}",
            slice.row_id
        );
        write_witness(&path, slice.channel_id, &slice.body, 99, now);
        assert!(
            delivery_record::recent_delivered_content_matches(
                &ProviderKind::Claude,
                ChannelId::new(slice.channel_id),
                &slice.tmux_session,
                &slice.body,
            ),
            "the live wrapper must still use the current generation"
        );
    }
    assert_eq!(
        plan,
        witness_plan(),
        "witness lookup cannot retire a planned slice"
    );
}

#[test]
fn dlq_witness_expired_missing_or_unreadable_keeps_slices_unsettled() {
    let root = tempfile::tempdir().unwrap();
    let _root_guard = crate::config::set_agentdesk_root_for_test(root.path());
    let plan = witness_plan();
    let path = delivery_record::delivery_record_path(&ProviderKind::Claude, 5551).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    for slice in &plan.slices {
        write_witness(&path, slice.channel_id, &slice.body, 42, now);
        assert!(
            slice_has_witness(slice),
            "a fresh original-generation witness must match before failure injection"
        );
        fs::remove_file(&path).unwrap();
    }
    let mut failures = Vec::new();
    for failure in ["missing", "expired", "malformed", "read-error"] {
        for slice in &plan.slices {
            match failure {
                "missing" => assert!(!path.exists()),
                "expired" => write_witness(&path, slice.channel_id, &slice.body, 42, 1),
                "malformed" => fs::write(&path, b"{broken").unwrap(),
                "read-error" => fs::create_dir(&path).unwrap(),
                _ => unreachable!(),
            }
            failures.push((failure, slice.row_id, slice_has_witness(slice)));
            assert_eq!(
                plan,
                witness_plan(),
                "{failure} cannot retire an obligation"
            );
            if path.is_dir() {
                fs::remove_dir(&path).unwrap();
            } else if path.exists() {
                fs::remove_file(&path).unwrap();
            }
        }
    }
    assert!(
        failures.iter().all(|(_, _, delivered)| !delivered),
        "failures are not delivery proofs: {failures:?}"
    );
}

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
    assert!(
        plan.slices
            .iter()
            .all(|slice| slice.generation_mtime_ns == 42)
    );
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
    let mut seen: Vec<(i64, u64, u64, i64)> = plan
        .slices
        .iter()
        .map(|s| {
            (
                s.row_id,
                s.channel_id,
                s.anchor_message_id,
                s.generation_mtime_ns,
            )
        })
        .collect();
    seen.sort_unstable();
    assert_eq!(
        seen,
        vec![
            (1, 5551, 7001, 42),
            (2, 6662, 8002, 42),
            (3, 5551, 7003, 99),
            (4, 5551, 7004, 42)
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
        row(
            5,
            "5551",
            Some("7001"),
            "tail",
            reason_for(0, 4, 42).replace("generation_mtime_ns=42", ""),
        ),
    ]);
    assert!(plan.slices.is_empty(), "none of these rows may be posted");
    let mut unaddressable = plan.unaddressable.clone();
    unaddressable.sort_unstable();
    assert_eq!(unaddressable, vec![1, 2, 3, 4, 5]);
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
    assert!(
        plan.slices
            .iter()
            .all(|slice| slice.generation_mtime_ns == 42)
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
