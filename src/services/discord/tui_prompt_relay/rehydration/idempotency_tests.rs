use super::*;
use crate::services::discord::inflight::{
    GuardedSaveOutcome, InflightTurnIdentity, load_inflight_state_read_only, save_inflight_state,
    save_stream_tick_state_if_bridge_authority,
};
use crate::services::tui_prompt_dedupe as dedupe;
use std::sync::Barrier;
use std::sync::atomic::AtomicUsize;

const SESSION: &str = "AgentDesk-codex-5755-idempotency";
const CHANNEL: u64 = 5755;

thread_local! { static BOOTSTRAP_PAUSE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) }; }

pub(super) fn bootstrap_pause(_tmux: &str) {
    BOOTSTRAP_PAUSE.with(|pause| {
        if let Some(action) = pause.borrow_mut().take() {
            action();
        }
    });
}

fn fixture(test: impl FnOnce(&Path)) {
    let temp = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(temp.path());
    let _dedupe = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    dedupe::reset_state_for_tests();
    test(temp.path());
    dedupe::reset_state_for_tests();
}

fn rollout(root: &Path, id: &str) -> PathBuf {
    let path = root.join(format!("{id}.jsonl"));
    std::fs::write(
        &path,
        format!("{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\"}}}}\n"),
    )
    .unwrap();
    crate::services::codex_tui::session::write_codex_tui_rollout_marker(SESSION, &path, Some(id))
        .unwrap();
    path
}

fn rehydrate(observe: impl FnOnce()) -> Option<dedupe::TuiRuntimeBinding> {
    rehydrate_codex_tui_binding_transaction(
        SESSION,
        CHANNEL,
        &claimed_codex_tui_rollout_paths(),
        &HashSet::new(),
        &HashSet::new(),
        false,
        observe,
    )
}

#[test]
fn codex_restore_leaves_a_valid_partial_user_at_the_complete_boundary() {
    fixture(|root| {
        let path = rollout(root, "boundary");
        let history = "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"historical answer\"}]}}\n";
        let partial = "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"next prompt\"}]}}";
        let header = std::fs::read_to_string(&path).unwrap();
        let boundary = (header.len() + history.len()) as u64;
        std::fs::write(&path, format!("{header}{history}{partial}")).unwrap();
        let binding = rehydrate(|| {}).expect("actual restore transaction");
        assert_eq!(
            binding.last_offset, boundary,
            "restore sealed a partial record"
        );
        assert!(binding.last_offset < std::fs::metadata(&path).unwrap().len());
    });
}

#[test]
fn codex_idle_scanner_requires_the_user_record_newline() {
    fixture(|root| {
        let path = rollout(root, "partial");
        let boundary = std::fs::metadata(&path).unwrap().len();
        let partial = "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"next prompt\"}]}}";
        let header = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, format!("{header}{partial}")).unwrap();
        let scan =
            super::super::idle_transcript_scan::scan_codex_idle_rollout_for_prompt(&path, boundary)
                .unwrap();
        assert!(
            matches!(scan, CodexIdleRolloutScan::NoPrompt { offset, .. } if offset == boundary),
            "unsealed prompt was admitted: {scan:?}"
        );
        std::fs::write(&path, format!("{header}{partial}\n")).unwrap();
        let scan =
            super::super::idle_transcript_scan::scan_codex_idle_rollout_for_prompt(&path, boundary)
                .unwrap();
        assert!(
            matches!(scan, CodexIdleRolloutScan::Prompt { prompt, .. } if prompt == "next prompt")
        );
    });
}

#[test]
fn second_rehydration_is_noop_and_preserves_stream_row_identity() {
    fixture(|root| {
        let path = rollout(root, "first");
        let first = rehydrate(|| {}).expect("initial restore");
        let mut progressed = first.clone();
        progressed.last_offset = std::fs::metadata(&path).unwrap().len();
        progressed.relay_last_offset = Some(11);
        dedupe::register_tmux_runtime_binding(SESSION, progressed.clone());
        let mut state = InflightTurnState::new(
            ProviderKind::Codex,
            CHANNEL,
            None,
            123,
            456,
            789,
            "working".into(),
            Some("first".into()),
            Some(SESSION.into()),
            Some(path.display().to_string()),
            None,
            progressed.last_offset,
        );
        state.runtime_kind = Some(RuntimeHandoffKind::CodexTui);
        state.full_response = "partial answer".into();
        save_inflight_state(&state).unwrap();
        let before = load_inflight_state_read_only(&ProviderKind::Codex, CHANNEL).unwrap();
        let second = rehydrate(|| panic!("valid binding must not be re-registered"));
        let after = load_inflight_state_read_only(&ProviderKind::Codex, CHANNEL)
            .expect("active stream row must survive rehydration");
        assert_eq!(
            InflightTurnIdentity::from_state(&after),
            InflightTurnIdentity::from_state(&before)
        );
        assert_eq!(
            serde_json::to_value(&after).unwrap(),
            serde_json::to_value(&before).unwrap()
        );
        assert_eq!(
            dedupe::runtime_binding_for_tmux_session(SESSION),
            Some(progressed)
        );
        assert!(
            second.is_none(),
            "second successful observation must be a no-op, not another rehydration"
        );
        let identity = InflightTurnIdentity::from_state(&before);
        let (msg, len) = (before.current_msg_id, before.current_msg_len);
        let mut baseline = before.clone();
        let mut streaming = before;
        streaming.full_response.push_str(" continues");
        assert_eq!(
            save_stream_tick_state_if_bridge_authority(
                &mut baseline,
                &mut streaming,
                &identity,
                msg,
                len,
                "rehydration::idempotency_tests::streaming_edit",
            ),
            GuardedSaveOutcome::Saved,
            "the active bridge must retain its guarded streaming save authority"
        );
    });
}

#[test]
fn codex_6903_raw_marker_migrates_once_without_sealing_the_suffix() {
    fixture(|root| {
        let path = rollout(root, "raw-marker");
        let boundary = std::fs::metadata(&path).unwrap().len();
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.extend_from_slice(b"{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"partial\"}]}}");
        std::fs::write(&path, &bytes).unwrap();
        crate::services::codex_tui::session::write_codex_tui_rollout_marker_with_start_offset(
            SESSION,
            &path,
            Some("raw-marker"),
            Some(bytes.len() as u64),
        )
        .unwrap();
        let binding = rehydrate(|| {}).unwrap();
        assert_eq!(binding.last_offset, boundary);
        assert_eq!(
            crate::services::codex_tui::session::read_codex_tui_rollout_marker(SESSION)
                .unwrap()
                .rollout_start_offset,
            Some(boundary)
        );
        let observed = dedupe::codex_restore_observation_for_tests(SESSION).unwrap();
        assert_eq!(observed.3.source_bytes, boundary);
        assert!(rehydrate(|| panic!("repeat publication")).is_none());
        assert_eq!(
            dedupe::codex_restore_observation_for_tests(SESSION)
                .unwrap()
                .3,
            observed.3
        );
    });
}

#[test]
fn codex_6903_source_replaced_during_bootstrap_stays_pending() {
    fixture(|root| {
        let path = rollout(root, "replace");
        let replace = path.clone();
        BOOTSTRAP_PAUSE.with(|pause| {
            *pause.borrow_mut() = Some(Box::new(move || {
                std::fs::rename(&replace, replace.with_extension("old")).unwrap();
                std::fs::write(
                    &replace,
                    "{\"type\":\"session_meta\",\"payload\":{\"id\":\"replace\"}}\n",
                )
                .unwrap();
            }))
        });
        assert!(rehydrate(|| {}).is_none());
        assert!(dedupe::runtime_binding_for_tmux_session(SESSION).is_none());
        assert!(dedupe::codex_restore_observation_for_tests(SESSION).is_none());
    });
}

#[test]
fn codex_6903_bootstrap_measures_complete_utf8_records_and_not_partial_bytes() {
    fixture(|root| {
        let path = rollout(root, "counts");
        let header = std::fs::read_to_string(&path).unwrap();
        let assistant = "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"한🙂\"}]}}\n";
        let event = "{\"type\":\"event_msg\",\"payload\":{\"type\":\"agent_message\",\"message\":\"한🙂\"}}\n";
        let tool = "{\"type\":\"response_item\",\"payload\":{\"type\":\"function_call\",\"role\":\"assistant\",\"content\":[{\"text\":\"tool payload\"}]}}\n";
        let full = format!("{header}{assistant}{event}{tool}malformed\n");
        std::fs::write(&path, format!("{full}{{\"type\":\"response_item\"}}")).unwrap();
        let binding = rehydrate(|| {}).unwrap();
        assert_eq!(binding.last_offset, full.len() as u64);
        let stats = dedupe::codex_restore_observation_for_tests(SESSION)
            .unwrap()
            .3;
        assert_eq!(stats.records, 5);
        assert_eq!(stats.assistant_records, 2);
        assert_eq!(stats.assistant_bytes, 2 * "한🙂".len() as u64);
        assert_eq!(stats.source_bytes, full.len() as u64);
        assert_eq!(stats.malformed, 1);
    });
}

#[test]
fn codex_6903_unknown_span_counts_only_an_actual_checkpoint_advance() {
    fixture(|root| {
        use crate::services::codex_tui::session::{
            advance_codex_tui_checkpoint, checkpoint::Advance,
        };
        let path = rollout(root, "advance");
        let binding = rehydrate(|| {}).unwrap();
        let body = "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"unknown\"}]}}\n";
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(body.as_bytes())
            .unwrap();
        let (scan, stats) = super::super::idle_transcript_scan::scan_codex_records(
            std::fs::File::open(&path).unwrap(),
            binding.last_offset,
            None,
            true,
            binding.last_offset,
        )
        .unwrap();
        let CodexIdleRolloutScan::NoPrompt { offset } = scan else {
            panic!("no input expected");
        };
        let result =
            advance_codex_tui_checkpoint(SESSION, &path, Some(binding.last_offset), offset);
        let Advance::Advanced { start, end, .. } = result else {
            panic!("actual span: {result:?}");
        };
        dedupe::record_codex_unknown_span(
            SESSION,
            start,
            end,
            stats,
            crate::services::codex_tui::session::checkpoint::ObservationKind::Unknown,
        );
        dedupe::record_codex_unknown_span(
            SESSION,
            start,
            end,
            stats,
            crate::services::codex_tui::session::checkpoint::ObservationKind::Unknown,
        );
        assert_eq!(
            advance_codex_tui_checkpoint(SESSION, &path, Some(start), end),
            Advance::StartMismatch
        );
        let totals = dedupe::codex_restore_observation_for_tests(SESSION)
            .unwrap()
            .3;
        assert_eq!(totals.assistant_bytes, "unknown".len() as u64);
        assert_eq!(totals.records, 2);
        let more = "{\"type\":\"padding\"}\n";
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(more.as_bytes())
            .unwrap();
        let marker = crate::services::tmux_common::session_temp_path(
            SESSION,
            crate::services::tmux_common::CODEX_TUI_ROLLOUT_MARKER_TEMP_EXT,
        );
        std::fs::rename(&marker, format!("{marker}.saved")).unwrap();
        std::fs::create_dir(&marker).unwrap();
        assert_eq!(
            advance_codex_tui_checkpoint(SESSION, &path, Some(end), end + more.len() as u64),
            Advance::PersistFailed
        );
        assert_eq!(
            dedupe::runtime_binding_for_tmux_session(SESSION)
                .unwrap()
                .last_offset,
            end
        );
        assert_eq!(
            dedupe::codex_restore_observation_for_tests(SESSION)
                .unwrap()
                .3,
            totals
        );
        std::fs::remove_dir(&marker).unwrap();
        std::fs::rename(format!("{marker}.saved"), &marker).unwrap();
        let old = path.with_extension("old");
        std::fs::rename(&path, &old).unwrap();
        std::fs::copy(&old, &path).unwrap();
        assert_eq!(
            advance_codex_tui_checkpoint(SESSION, &path, Some(end), end + more.len() as u64),
            Advance::SourceChanged
        );
        assert_eq!(
            dedupe::runtime_binding_for_tmux_session(SESSION)
                .unwrap()
                .last_offset,
            end
        );
        assert_eq!(
            dedupe::codex_restore_observation_for_tests(SESSION)
                .unwrap()
                .3,
            totals
        );
    });
}

#[test]
fn live_marker_rotation_replaces_even_when_old_rollout_still_exists() {
    fixture(|root| {
        let old = rollout(root, "old");
        rehydrate(|| {}).unwrap();
        let new = rollout(root, "new");
        let replaced = AtomicUsize::new(0);
        let fresh = rehydrate(|| {
            replaced.fetch_add(1, Ordering::SeqCst);
        })
        .unwrap();
        assert!(old.exists());
        assert_eq!(fresh.output_path, new.display().to_string());
        assert_eq!(fresh.session_id.as_deref(), Some("new"));
        assert_eq!(replaced.load(Ordering::SeqCst), 1);
        assert!(rehydrate(|| panic!("same replacement must settle")).is_none());
    });
}

#[test]
fn namespace_change_repairs_relay_path_without_rewinding_rollout() {
    fixture(|root| {
        rollout(root, "same-session");
        let mut previous = rehydrate(|| {}).unwrap();
        let expected_offset = previous.last_offset;
        previous.relay_output_path =
            Some(root.join("previous-namespace.jsonl").display().to_string());
        dedupe::register_tmux_runtime_binding(SESSION, previous);
        let repaired = rehydrate(|| {}).expect("namespace repair");
        assert_eq!(repaired.last_offset, expected_offset);
        assert_eq!(
            repaired.relay_output_path,
            Some(crate::services::tmux_common::session_temp_path(
                SESSION, "jsonl"
            ))
        );
        assert!(rehydrate(|| panic!("namespace repair must settle")).is_none());
    });
}

#[test]
fn recreated_tmux_with_evicted_binding_can_rehydrate_same_rollout() {
    fixture(|root| {
        rollout(root, "surviving-rollout");
        let first = rehydrate(|| {}).unwrap();
        assert!(dedupe::evict_dead_tmux_mirror(SESSION));
        let restored = rehydrate(|| {}).expect("new pane may restore the surviving rollout");
        assert_eq!(restored.output_path, first.output_path);
        assert!(rehydrate(|| panic!("recreated pane must settle")).is_none());
    });
}

#[test]
fn concurrent_rehydrations_install_once() {
    fixture(|root| {
        rollout(root, "concurrent");
        let ready = Barrier::new(2);
        let installs = AtomicUsize::new(0);
        let restored = std::thread::scope(|scope| {
            let run = || {
                ready.wait();
                rehydrate(|| {
                    assert!(
                        crate::services::tmux_common::try_with_tmux_source_authority(
                            SESSION,
                            |_| ()
                        )
                        .is_none()
                    );
                    installs.fetch_add(1, Ordering::SeqCst);
                })
                .is_some()
            };
            let left = scope.spawn(run);
            let right = scope.spawn(run);
            usize::from(left.join().unwrap()) + usize::from(right.join().unwrap())
        });
        assert_eq!(installs.load(Ordering::SeqCst), 1);
        assert_eq!(restored, 1, "only the winning call may report rehydration");
    });
}

#[test]
fn unusable_marker_preserves_valid_binding_and_cursor() {
    fixture(|root| {
        rollout(root, "valid");
        let existing = rehydrate(|| {}).unwrap();
        let foreign = rollout(root, "foreign");
        for (claims, duplicates) in [
            (
                HashSet::from([canonical_rollout_claim_path(&foreign)]),
                HashSet::new(),
            ),
            (
                HashSet::new(),
                HashSet::from([canonical_rollout_claim_path(&foreign)]),
            ),
        ] {
            assert!(
                rehydrate_codex_tui_binding_transaction(
                    SESSION,
                    CHANNEL,
                    &claims,
                    &HashSet::new(),
                    &duplicates,
                    false,
                    || panic!("foreign or duplicate marker must not replace a valid binding"),
                )
                .is_none()
            );
            assert_eq!(
                dedupe::runtime_binding_for_tmux_session(SESSION),
                Some(existing.clone())
            );
        }
        std::fs::remove_file(foreign).unwrap();
        assert!(rehydrate(|| panic!("stale marker must not replace a valid binding")).is_none());
        assert_eq!(
            dedupe::runtime_binding_for_tmux_session(SESSION),
            Some(existing)
        );
    });
}

#[test]
fn rollout_alias_and_channel_mirror_repair_preserve_binding() {
    fixture(|root| {
        let path = rollout(root, "aliased");
        let existing = rehydrate(|| {}).unwrap();
        let observed = dedupe::codex_restore_observation_for_tests(SESSION).unwrap();
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"type\":\"padding\"}\n")
            .unwrap();
        let alias = root.join("alias.jsonl");
        std::os::unix::fs::symlink(path, &alias).unwrap();
        crate::services::codex_tui::session::write_codex_tui_rollout_marker(
            SESSION,
            &alias,
            Some("aliased"),
        )
        .unwrap();
        dedupe::register_tmux_channel(SESSION, CHANNEL + 1);
        assert!(
            rehydrate(|| panic!("alias and channel mirror repair must not re-register source"))
                .is_none()
        );
        assert_eq!(
            dedupe::owner_channel_for_tmux_session(SESSION),
            Some(CHANNEL)
        );
        assert_eq!(
            dedupe::runtime_binding_for_tmux_session(SESSION),
            Some(existing)
        );
        assert_eq!(
            dedupe::codex_restore_observation_for_tests(SESSION).unwrap(),
            observed,
            "an alias repair must preserve the same source bootstrap boundary and observation"
        );
    });
}

#[test]
fn incomplete_session_metadata_does_not_reset_a_valid_rollout_cursor() {
    fixture(|root| {
        rollout(root, "metadata");
        let mut existing = rehydrate(|| {}).unwrap();
        existing.session_id = None;
        dedupe::register_tmux_runtime_binding(SESSION, existing.clone());
        assert!(
            rehydrate(|| panic!("learning optional metadata must not reset the source")).is_none()
        );
        assert_eq!(
            dedupe::runtime_binding_for_tmux_session(SESSION),
            Some(existing)
        );
    });
}

#[derive(Clone, Default)]
struct LogSink(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for LogSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogSink {
    type Writer = LogSink;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Pins the Codex periodic pass's tmux view for this thread and clears it on unwind.
struct PinnedTmuxView;

impl PinnedTmuxView {
    fn live(sessions: &[&str]) -> Self {
        CODEX_PASS_TMUX_VIEW.set(Some(sessions.iter().map(ToString::to_string).collect()));
        Self
    }
}

impl Drop for PinnedTmuxView {
    fn drop(&mut self) {
        CODEX_PASS_TMUX_VIEW.set(None);
    }
}

/// Runs one production periodic pass and counts its success logs.
fn entry_pass_recoveries(shared: &Arc<SharedData>) -> usize {
    let sink = LogSink::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_ansi(false)
        .without_time()
        .with_writer(sink.clone())
        .finish();
    crate::logging::test_capture::pin_callsite_interest();
    tracing::subscriber::with_default(subscriber, || {
        rehydrate_existing_codex_tui_bindings(shared);
    });
    String::from_utf8(sink.0.lock().unwrap().clone())
        .unwrap()
        .matches("rehydrated Codex TUI direct relay binding from live rollout")
        .count()
}

#[test]
fn periodic_entry_pass_reports_one_recovery_then_settles() {
    fixture(|root| {
        let session = "adk-rehydrate-entry";
        let _tmux = PinnedTmuxView::live(&[session]);
        crate::services::tmux_common::write_tmux_runtime_kind_marker(
            session,
            RuntimeHandoffKind::CodexTui,
        )
        .unwrap();
        let path = root.join("entry.jsonl");
        std::fs::write(
            &path,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"entry\"}}\n",
        )
        .unwrap();
        crate::services::codex_tui::session::write_codex_tui_rollout_marker(
            session,
            &path,
            Some("entry"),
        )
        .unwrap();
        dedupe::register_tmux_channel(session, CHANNEL);
        let shared = crate::services::discord::make_shared_data_for_tests();

        assert_eq!(
            entry_pass_recoveries(&shared),
            1,
            "missing binding restores"
        );
        let mut progressed =
            dedupe::runtime_binding_for_tmux_session(session).expect("restored binding");
        assert_eq!(progressed.output_path, path.display().to_string());
        progressed.last_offset = std::fs::metadata(&path).unwrap().len();
        progressed.relay_last_offset = Some(11);
        dedupe::register_tmux_runtime_binding(session, progressed.clone());
        let written = dedupe::runtime_binding_recorded_at_for_tests(session);

        for _ in 0..3 {
            assert_eq!(
                entry_pass_recoveries(&shared),
                0,
                "a valid binding must not be reported as rehydrated again"
            );
        }
        assert_eq!(
            dedupe::runtime_binding_recorded_at_for_tests(session),
            written,
            "a settled pass must not rewrite the binding, even with the same value"
        );
        assert_eq!(
            dedupe::runtime_binding_for_tmux_session(session),
            Some(progressed)
        );
    });
}

#[test]
fn codex_6903_restore_without_a_floor_preserves_the_complete_boundary_and_ttl() {
    fixture(|root| {
        let path = rollout(root, "floor-pending");
        let b = std::fs::metadata(&path).unwrap().len();
        std::mem::forget(dedupe::reserve_discord_handoff("codex", SESSION, "origin"));
        assert_eq!(
            dedupe::observe_codex_prompt_in_turn_at(
                SESSION,
                "parked",
                None,
                Some("turn-p"),
                chrono::Utc::now()
            ),
            crate::services::tui_prompt_dedupe::PromptObservation::ParkedForHandoff
        );
        let held = root
            .join("runtime/codex_handoff_held")
            .join(format!("codex-{SESSION}.json"));
        let before: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&held).unwrap()).unwrap();
        dedupe::reset_state_for_tests();
        let binding = rehydrate(|| {}).unwrap();
        assert_eq!(binding.last_offset, b);
        assert_eq!(
            dedupe::codex_restore_observation_for_tests(SESSION)
                .unwrap()
                .2,
            Some(b)
        );
        assert_eq!(
            dedupe::held_handoff_inputs_for_tests("codex", SESSION),
            Some(1)
        );
        dedupe::drain_expired_handoffs();
        assert_eq!(
            dedupe::held_handoff_inputs_for_tests("codex", SESSION),
            Some(1)
        );
        let after: serde_json::Value =
            serde_json::from_slice(&std::fs::read(held).unwrap()).unwrap();
        assert_eq!(
            after["expires_at"], before["expires_at"],
            "restore must not renew remaining TTL"
        );
    });
}

#[test]
fn codex_6903_floor_appended_after_bootstrap_starts_at_the_fixed_boundary() {
    fixture(|root| {
        let path = rollout(root, "future-floor");
        let b = std::fs::metadata(&path).unwrap().len();
        std::mem::forget(dedupe::reserve_discord_handoff("codex", SESSION, "origin"));
        assert_eq!(
            dedupe::observe_codex_prompt_in_turn_at(
                SESSION,
                "parked",
                None,
                Some("turn-p"),
                chrono::Utc::now()
            ),
            crate::services::tui_prompt_dedupe::PromptObservation::ParkedForHandoff
        );
        dedupe::reset_state_for_tests();
        let append = path.clone();
        BOOTSTRAP_PAUSE.with(|pause| *pause.borrow_mut() = Some(Box::new(move || {
            use std::io::Write;
            let pad = serde_json::json!({"type":"padding"}).to_string() + "\n";
            let user = serde_json::json!({"timestamp":chrono::Utc::now().to_rfc3339(),"type":"response_item",
                "payload":{"type":"message","role":"user","turn_id":"origin-turn","content":[{"type":"input_text","text":"origin"}]}}).to_string() + "\n";
            std::fs::OpenOptions::new().append(true).open(append).unwrap().write_all((pad + &user).as_bytes()).unwrap();
        })));
        let binding = rehydrate(|| {}).unwrap();
        assert_eq!(
            binding.last_offset, b,
            "future floor must not skip the newly appended prefix"
        );
        let seen = dedupe::codex_restore_observation_for_tests(SESSION).unwrap();
        assert_eq!(seen.0, b);
        assert_eq!(seen.2, Some(b));
        assert_eq!(seen.3.source_bytes, b);
    });
}

#[test]
fn codex_6903_truncate_and_conflicting_custody_source_stay_pending() {
    fixture(|root| {
        let path = rollout(root, "source-pending");
        let replace = path.clone();
        BOOTSTRAP_PAUSE.with(|pause| {
            *pause.borrow_mut() = Some(Box::new(move || {
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(replace)
                    .unwrap()
                    .set_len(0)
                    .unwrap();
            }))
        });
        assert!(rehydrate(|| {}).is_none());
        assert!(dedupe::runtime_binding_for_tmux_session(SESSION).is_none());
    });
    fixture(|root| {
        let path = rollout(root, "source-conflict");
        std::mem::forget(dedupe::reserve_discord_handoff("codex", SESSION, "origin"));
        dedupe::observe_codex_prompt_in_turn_at(
            SESSION,
            "parked",
            None,
            Some("turn-p"),
            chrono::Utc::now(),
        );
        let other = root.join("other.jsonl");
        std::fs::copy(&path, &other).unwrap();
        let row = crate::services::discord::inflight::InflightTurnState::new(
            ProviderKind::Codex,
            CHANNEL,
            None,
            123,
            456,
            789,
            "origin".into(),
            Some("source-conflict".into()),
            Some(SESSION.into()),
            Some(other.display().to_string()),
            None,
            0,
        );
        save_inflight_state(&row).unwrap();
        assert!(
            rehydrate(|| {}).is_none(),
            "another known custody source cannot bind this rollout"
        );
        assert!(dedupe::runtime_binding_for_tmux_session(SESSION).is_none());
        assert!(dedupe::durable_handoff_present_for_tests("codex", SESSION));
    });
}
