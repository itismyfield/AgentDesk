//! Main baselines, losses included: what the delivery frontier does when a dropped terminal
//! body sits under a later commit. A message saying "loss" marks a value the fix should flip.

use super::*;

struct StrandedFrontierFixture {
    _root_guard: crate::config::TestEnvVarGuard,
    _root: tempfile::TempDir,
    shared: Arc<SharedData>,
    channel: serenity::ChannelId,
    tmux: &'static str,
}

impl StrandedFrontierFixture {
    fn new(channel: u64, tmux: &'static str, generation_unix_secs: i64) -> Self {
        let root = tempfile::TempDir::new().expect("isolated runtime root");
        let root_guard = crate::config::set_agentdesk_root_for_test(root.path());
        let shared = crate::services::discord::make_shared_data_for_tests();
        let generation_path = crate::services::tmux_common::session_temp_path(tmux, "generation");
        std::fs::create_dir_all(std::path::Path::new(&generation_path).parent().unwrap()).unwrap();
        std::fs::write(&generation_path, "i6292").unwrap();
        filetime::set_file_mtime(
            &generation_path,
            filetime::FileTime::from_unix_time(generation_unix_secs, 0),
        )
        .unwrap();
        Self {
            _root_guard: root_guard,
            _root: root,
            shared,
            channel: serenity::ChannelId::new(channel),
            tmux,
        }
    }

    /// A confirmed watcher-direct delivery of `range` through the production funnel.
    fn commit(&self, range: (u64, u64), body: &str) {
        let identity = super::super::terminal_long_chunks::watcher_delivery_identity(
            dr::current_generation_mtime_ns(self.tmux),
            self.shared
                .relay_frontier_token(self.channel)
                .reset_incarnation,
            None,
        );
        let result = super::super::terminal_long_chunks::record_watcher_terminal_delivery(
            crate::services::discord::tmux::WatcherDeliveryTarget {
                shared: &self.shared,
                provider: &ProviderKind::Claude,
                channel_id: self.channel,
                tmux_session_name: self.tmux,
            },
            identity,
            range,
            Some(range.1),
            body,
        );
        assert_eq!(
            result,
            super::super::terminal_long_chunks::GuardedWatcherDeliveryResult::Persisted,
            "fixture commit {range:?} must reach the durable record"
        );
    }

    /// The orphan-frame seam dropping `[start, end)` with a non-empty body.
    fn drop_terminal(&self, start: u64, end: u64, committed_floor: u64, body: &'static str) {
        let facts = OrphanTerminalFrameFacts {
            current_response: body,
            response_sent_offset: 0,
            full_response_len: body.len(),
            data_start_offset: start,
            current_offset: end,
            terminal_event_consumed_offset: end,
            watcher_resend_committed: committed_floor,
            tmux_session_name: self.tmux,
            ..orphan_facts()
        };
        assert!(
            facts.record_required(),
            "the drop must be the I17 loss shape"
        );
        let _ = observe_orphan_terminal_frame(
            &self.shared,
            self.channel,
            &ProviderKind::Claude,
            &facts,
        );
    }

    fn durable_end(&self) -> Option<u64> {
        dr::resolved_delivered_frontier_end_current_generation(
            &ProviderKind::Claude,
            self.channel,
            self.tmux,
            Some(u64::MAX / 2),
        )
    }

    fn in_memory_end(&self) -> u64 {
        self.shared
            .tmux_relay_coord(self.channel)
            .confirmed_end_offset
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Durable and in-memory frontier END, pinned together.
    fn assert_frontier(&self, end: u64, context: &str) {
        assert_eq!(self.durable_end(), Some(end), "durable frontier {context}");
        assert_eq!(self.in_memory_end(), end, "in-memory frontier {context}");
    }

    /// A sparse transcript registered as this session's watcher output, so the
    /// EOF-bounded frontier readers see every fixture offset in bounds.
    fn register_transcript(&self) -> String {
        let path = self._root.path().join(format!("{}.jsonl", self.tmux));
        std::fs::File::create(&path)
            .and_then(|file| file.set_len(46_000_000))
            .expect("sparse transcript");
        let path = path.to_string_lossy().into_owned();
        self.shared.tmux_watchers.insert(
            self.channel,
            crate::services::discord::TmuxWatcherHandle {
                tmux_session_name: self.tmux.to_string(),
                output_path: path.clone(),
                paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                resume_offset: Arc::new(std::sync::Mutex::new(None)),
                cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                pause_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                turn_delivered: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                last_heartbeat_ts_ms: Arc::new(std::sync::atomic::AtomicI64::new(
                    crate::services::discord::tmux_watcher_now_ms(),
                )),
            },
        );
        path
    }
}

fn assistant_text(body: &str) -> String {
    format!(
        "{}\n",
        serde_json::json!({"type": "assistant", "message": {"role": "assistant", "content": [{"type": "text", "text": body}]}})
    )
}

const STOP_HOOK: &str =
    "{\"type\":\"system\",\"subtype\":\"stop_hook_summary\",\"sessionId\":\"s\"}\n";

/// the next turn's commit carries the frontier over the dropped T31,
/// and a later replay of T31 cannot move it back.
#[test]
fn a_dropped_turn_under_the_next_turn_commit_baseline_6292() {
    let fx =
        StrandedFrontierFixture::new(6_292_031, "AgentDesk-claude-i6292-next-turn", 1_790_406_292);
    let (f, t31_end, t32_end) = (44_941_631, 45_008_615, 45_040_962);
    fx.commit((44_700_261, f), "T30 body");
    fx.drop_terminal(f, t31_end, f, "T31 report the watcher dropped (1204 bytes)");
    fx.commit((t31_end, t32_end), "T32 body");
    fx.assert_frontier(t32_end, "is T32's END: loss, T31 [f, t31_end) absorbed");
    assert!(
        !super::rowless_delivery_authority::ledger_owes_output(t31_end, fx.durable_end()),
        "loss: no rowless replay owes T31"
    );

    fx.commit((f, t31_end), "T31 report delivered by a replay");
    fx.assert_frontier(t32_end, "stays at the END max after the T31 replay");
}

/// a redrive frame skips the dropped T20 as pre-turn output of the
/// live T21 row, and T21's commit absorbs T20's remainder.
#[test]
fn a_redrive_frame_that_skips_a_dropped_turn_baseline_6292() {
    let fx =
        StrandedFrontierFixture::new(6_292_020, "AgentDesk-claude-i6292-pre-turn", 1_790_406_220);
    let (t20_start, t20_end) = (44_026_150, 44_129_329);
    fx.commit((43_950_000, 43_966_655), "T18 body");
    fx.drop_terminal(
        t20_start,
        t20_end,
        43_966_655,
        "T20 decision report (916 bytes)",
    );
    fx.commit((43_966_655, 43_983_039), "T19 L24514");
    fx.commit((43_983_039, 43_999_423), "T19 L24533");
    fx.commit((43_999_423, 44_026_962), "T19 L24550");

    let t21_start = 44_137_073;
    let mut buffer = format!("{}{STOP_HOOK}", assistant_text("T20 decision report"));
    let pre_turn_len = (t21_start - 44_026_962) as usize;
    buffer.push_str(&" ".repeat(pre_turn_len - buffer.len() - 1));
    buffer.push('\n');
    buffer.push_str(&assistant_text("T21 answer"));
    let mut full_response = String::new();
    let outcome = crate::services::discord::tmux::process_watcher_lines_for_turn(
        &mut buffer,
        &mut crate::services::session_backend::StreamLineState::new(),
        &mut full_response,
        &mut crate::services::discord::tmux::WatcherToolState::new(),
        Some(44_026_962),
        Some(t21_start),
    );
    assert!(
        !full_response.contains("T20") && full_response.contains("T21 answer"),
        "the live T21 row makes the parser skip T20, got {full_response:?}"
    );
    assert_eq!(outcome.pre_turn_bytes_skipped, pre_turn_len);

    fx.commit((t21_start, 44_195_737), "T21 body (591 chars)");
    fx.assert_frontier(
        44_195_737,
        "is T21's END: loss, T20 remainder [44026962, 44129329) absorbed",
    );
    assert!(
        !super::rowless_delivery_authority::ledger_owes_output(t20_end, fx.durable_end()),
        "loss: no rowless replay owes T20"
    );
}

/// Guard: prompt and hook bytes between turns advance the frontier.
#[test]
fn a_gap_with_no_dropped_body_advances_the_frontier_baseline_6292() {
    let fx = StrandedFrontierFixture::new(
        6_292_099,
        "AgentDesk-claude-i6292-benign-gap",
        1_790_406_299,
    );
    fx.commit((40_800_000, 40_841_746), "previous turn");
    fx.commit((40_842_351, 40_907_659), "next turn");
    fx.assert_frontier(40_907_659, "crosses a gap with no dropped body");
}

/// the edit-failure recheck and the resend floor read T32's END and
/// answer that the dropped T31 range is already committed.
#[test]
fn an_edit_failure_recheck_over_a_dropped_turn_baseline_6292() {
    let fx = StrandedFrontierFixture::new(6_292_313, "AgentDesk-claude-i6292-edit", 1_790_406_293);
    let (f, t31_end, t32_end) = (44_941_631, 45_008_615, 45_040_962);
    let output_path = fx.register_transcript();
    fx.commit((44_700_261, f), "T30 body");
    fx.drop_terminal(f, t31_end, f, "T31 report the watcher dropped (1204 bytes)");
    fx.commit((t31_end, t32_end), "T32 body");

    let identity = dr::capture_edit_failure_transcript_identity(&fx.shared, fx.tmux);
    assert!(identity.is_some(), "the transcript identity is capturable");
    assert!(
        dr::range_committed_after_edit_failure(
            &fx.shared,
            &ProviderKind::Claude,
            fx.channel,
            fx.tmux,
            identity.as_ref(),
            t31_end,
        ),
        "loss: an edit failure on the T31 replay reconciles as AlreadyCommitted"
    );
    let eof = std::fs::metadata(&output_path).map(|meta| meta.len()).ok();
    let floor = dr::committed_floor_for_resend_dedup(
        &fx.shared,
        &ProviderKind::Claude,
        fx.channel,
        fx.tmux,
        eof,
    );
    assert_eq!(floor, t32_end, "the resend floor is T32's END");
    assert!(
        dr::range_already_committed(t31_end, floor),
        "loss: resend dedup skips the T31 replay as already committed"
    );
}

/// a drop straddling F never lowers it, and a later disjoint commit
/// carries the frontier over the owed remainder.
#[test]
fn a_drop_that_straddles_the_frontier_baseline_6292() {
    let fx =
        StrandedFrontierFixture::new(6_292_004, "AgentDesk-claude-i6292-straddle", 1_790_406_204);
    fx.commit((1_000, 2_000), "turn 1");
    fx.drop_terminal(1_500, 3_000, 2_000, "turn 2 body the watcher dropped");
    fx.assert_frontier(2_000, "does not move back below the confirmed prefix");
    fx.commit((3_000, 4_000), "turn 3");
    fx.assert_frontier(4_000, "is turn 3's END: loss, [2000, 3000) absorbed");
    fx.commit((2_000, 3_000), "turn 2 body replayed");
    fx.assert_frontier(4_000, "stays at the END max after the replay");
}

/// a partial cover holds, then a disjoint later commit absorbs the rest.
#[test]
fn split_commits_over_a_dropped_range_baseline_6292() {
    let fx = StrandedFrontierFixture::new(6_292_005, "AgentDesk-claude-i6292-split", 1_790_406_205);
    fx.commit((1_000, 1_500), "turn 1");
    fx.drop_terminal(1_900, 3_000, 1_500, "turn 2 body the watcher dropped");
    fx.commit((1_500, 2_200), "turn 1 tail plus the head of turn 2");
    fx.assert_frontier(2_200, "after a partial cover");
    fx.commit((4_000, 5_000), "turn 3");
    fx.assert_frontier(5_000, "is turn 3's END: loss, [2200, 3000) absorbed");
    fx.commit((2_200, 3_000), "rest of turn 2");
    fx.assert_frontier(5_000, "stays at the END max after the replay");
}

/// the first drop of a generation is owed to no rowless replay, and the
/// next turn's commit absorbs it.
#[test]
fn the_first_drop_of_a_generation_baseline_6292() {
    let fx = StrandedFrontierFixture::new(6_292_007, "AgentDesk-claude-i6292-first", 1_790_406_207);
    let output_path = fx.register_transcript();
    fx.drop_terminal(5_000, 9_000, 0, "first turn body the watcher dropped");
    let owed = |consumed_end| {
        super::rowless_delivery_authority::read_rowless_delivery_authority(
            &fx.shared,
            &ProviderKind::Claude,
            fx.channel,
            fx.tmux,
            &output_path,
            consumed_end,
        )
        .ledger_obligation_open
    };
    assert!(
        !owed(9_000),
        "loss: the dropped first turn is owed to no rowless replay"
    );
    fx.commit((9_000, 9_500), "second turn");
    assert_eq!(
        fx.durable_end(),
        Some(9_500),
        "loss: the second turn's commit absorbs [5000, 9000)"
    );
    assert!(!owed(9_000), "loss: still owed to no rowless replay");
}

/// a delivered turn committed twice, then observed late by the seam; the
/// frontier is at B's END throughout and a replay of A does not move it.
#[test]
fn a_late_registration_of_a_delivered_turn_baseline_6292() {
    let fx = StrandedFrontierFixture::new(6_292_033, "AgentDesk-claude-i6292-late", 1_790_406_233);
    fx.commit((500, 1_000), "turn 0");
    fx.drop_terminal(1_000, 2_000, 1_000, "turn A body the watcher dropped");
    fx.commit((2_000, 3_000), "turn B");
    fx.commit((2_000, 3_000), "turn B");
    fx.assert_frontier(3_000, "is B's END: loss, the owed turn A absorbed");
    fx.drop_terminal(2_000, 3_000, 1_000, "turn B body observed late by the seam");
    fx.assert_frontier(3_000, "is unchanged by the late observation");
    fx.commit((1_000, 2_000), "turn A replayed");
    fx.assert_frontier(3_000, "stays at the END max after the A replay");
}

/// each of forty disjoint drops is absorbed by the next delivered turn.
#[test]
fn many_disjoint_drops_baseline_6292() {
    let fx = StrandedFrontierFixture::new(6_292_040, "AgentDesk-claude-i6292-many", 1_790_406_240);
    fx.commit((0, 1_000), "turn 0");
    let drops: Vec<(u64, u64)> = (0..40u64)
        .map(|i| (1_000 + i * 2_000, 2_000 + i * 2_000))
        .collect();
    for &(start, end) in &drops {
        fx.drop_terminal(start, end, 1_000, "dropped body");
        fx.commit((end, end + 1_000), "delivered turn");
        fx.assert_frontier(end + 1_000, "loss: the drop just before it is absorbed");
    }
    for &(start, end) in drops.iter().rev() {
        fx.commit((start, end), "dropped body replayed");
    }
    fx.assert_frontier(81_000, "stays at the END max after every replay");
}

/// Hazard witness: a stop_hook_summary does not end the parse, so one
/// soft-terminal batch carries the owed turn A and the delivered turn B.
#[test]
fn a_soft_terminal_batch_carries_the_next_turn_baseline_6292() {
    let mut buffer = format!(
        "{}{STOP_HOOK}{}{STOP_HOOK}",
        assistant_text("owed turn A"),
        assistant_text("delivered turn B")
    );
    let mut full_response = String::new();
    let outcome = crate::services::discord::tmux::process_watcher_lines_for_turn(
        &mut buffer,
        &mut crate::services::session_backend::StreamLineState::new(),
        &mut full_response,
        &mut crate::services::discord::tmux::WatcherToolState::new(),
        Some(1_000),
        Some(1_000),
    );
    assert!(outcome.soft_terminal_candidate && !outcome.found_result);
    assert!(
        full_response.contains("owed turn A") && full_response.contains("delivered turn B"),
        "one soft-terminal batch carries both turns: {full_response:?}"
    );
}

/// the chunk nonce is a function of the turn key and chunk index only, so
/// two different bodies sent under one key share nonce 0.
#[test]
fn r3_1_chunk_nonce_ignores_the_body_and_range_baseline_6292() {
    use crate::services::discord::task_notification_delivery::response_chunk_nonce;
    let key = "owed:claude:6292:1790406292000000000:100:300";
    let first_range_chunk = response_chunk_nonce(key, 0);
    let second_range_chunk = response_chunk_nonce(key, 0);
    assert_eq!(
        first_range_chunk, second_range_chunk,
        "a [100,200) body and a [200,300) body under one origin key collide on chunk 0"
    );
    assert_ne!(first_range_chunk, response_chunk_nonce(key, 1));
}

/// the only anchor a ranged recovery can find is the frontier commit's;
/// a delivered range below the frontier's commit has no findable anchor.
#[test]
fn r3_4_recovery_anchor_is_only_the_frontier_commit_baseline_6292() {
    let fx =
        StrandedFrontierFixture::new(6_292_304, "AgentDesk-claude-i6292-anchor", 1_790_406_304);
    fx.commit((0, 100), "turn 0");
    fx.drop_terminal(100, 200, 100, "turn A body the watcher dropped");
    fx.commit((200, 300), "turn B");
    let anchor = || {
        crate::services::discord::outbound::delivery_frontier_probe::delivered_frontier_current_generation(
            &ProviderKind::Claude,
            fx.channel,
            fx.tmux,
            Some(u64::MAX / 2),
        )
        .map(|commit| commit.range)
    };
    assert_eq!(
        anchor(),
        Some((200, 300)),
        "B is the frontier commit on main"
    );
    fx.commit((300, 400), "turn C");
    assert_eq!(
        anchor(),
        Some((300, 400)),
        "after C, B's range no longer matches the one recorded anchor"
    );
}

/// main's committed check compares the range END only, so a straddle
/// `s < F < e` is not committed and an END of 0 never is.
#[test]
fn r3_6_range_already_committed_compares_the_end_only_baseline_6292() {
    assert!(
        !dr::range_already_committed(150, 100),
        "straddle [50,150) over F=100 is sent"
    );
    assert!(dr::range_already_committed(100, 100));
    assert!(dr::range_already_committed(60, 100));
    assert!(
        !dr::range_already_committed(0, 100),
        "END 0 is never committed"
    );
}

/// an unreadable generation marker reads as generation 0, which the
/// current-generation frontier reader treats as a different generation.
#[test]
fn r3_7_an_unreadable_generation_marker_reads_as_generation_zero_baseline_6292() {
    let fx =
        StrandedFrontierFixture::new(6_292_307, "AgentDesk-claude-i6292-marker", 1_790_406_307);
    fx.commit((0, 100), "turn 0");
    assert_ne!(dr::current_generation_mtime_ns(fx.tmux), 0);
    assert_eq!(fx.durable_end(), Some(100));
    let marker = crate::services::tmux_common::session_temp_path(fx.tmux, "generation");
    std::fs::remove_file(&marker).expect("remove the generation marker");
    assert_eq!(
        dr::current_generation_mtime_ns(fx.tmux),
        0,
        "a metadata failure collapses to generation 0"
    );
    assert_eq!(
        fx.durable_end(),
        None,
        "the committed frontier reads as a prior generation while the marker is unreadable"
    );
}
