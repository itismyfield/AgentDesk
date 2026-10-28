//! The intake save of a Codex raw seed: source recheck at save time and its lock order.

use super::*;
use crate::services::discord::inflight;
use crate::services::tui_prompt_dedupe::{self as dedupe, TuiRuntimeBinding};
use std::path::{Path, PathBuf};

fn install_source(tmux: &str, rollout: &Path, session: &str) {
    crate::services::codex_tui::session::write_codex_tui_rollout_marker_with_start_offset(
        tmux,
        rollout,
        Some(session),
        Some(0),
    )
    .unwrap();
    dedupe::register_tmux_runtime_binding(
        tmux,
        TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::CodexTui,
            output_path: rollout.display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: Some(session.to_string()),
            last_offset: std::fs::metadata(rollout).unwrap().len(),
            relay_last_offset: None,
        },
    );
}

fn seeded_row(channel: u64, tmux: &str, seed: &CodexRawSeed) -> InflightTurnState {
    let (output_path, input_fifo_path) = tmux_runtime_paths(tmux);
    let (tmux_name, path, fifo, offset) = prelaunch_inflight_runtime_seed_from_paths(
        tmux,
        output_path,
        input_fifo_path,
        true,
        Some(RuntimeHandoffKind::CodexTui),
        Some((seed.path.clone(), seed.end)),
    );
    let mut state = InflightTurnState::new(
        ProviderKind::Codex,
        channel,
        None,
        1,
        2,
        3,
        "prompt".to_string(),
        Some("intake-session".to_string()),
        tmux_name,
        path,
        fifo,
        offset,
    );
    apply_prelaunch_runtime_kind(&mut state, Some(RuntimeHandoffKind::CodexTui));
    state
}

fn canonical(path: &Path) -> String {
    std::fs::canonicalize(path).unwrap().display().to_string()
}

fn saved(channel: u64) -> (Option<String>, u64, Option<u64>, Option<String>) {
    let row = inflight::load_inflight_state(&ProviderKind::Codex, channel).expect("saved row");
    (
        row.output_path,
        row.last_offset,
        row.turn_start_offset,
        row.session_id,
    )
}

// A source replaced between the seed read and the save must not reach the row; an unreplaced
// source keeps its seed even after its rollout grew past the binding offset.
#[test]
fn codex_seed_replaced_before_persist_is_not_written() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    dedupe::run_serial_test(async {
        dedupe::reset_state_for_tests();
        let dir = tempfile::tempdir().unwrap();
        let rollout = |name: &str, bytes: usize| -> PathBuf {
            let path = dir.path().join(name);
            std::fs::write(&path, vec![b'x'; bytes]).unwrap();
            path
        };

        let tmux = "AgentDesk-codex-seed-replaced";
        let (a, b) = (rollout("a.jsonl", 11), rollout("b.jsonl", 13));
        install_source(tmux, &a, "session-a");
        let seed = observed_codex_tui_raw_seed(tmux).expect("observed seed of source A");
        assert_eq!((seed.path.as_str(), seed.end), (canonical(&a).as_str(), 11));
        let wrapper = tmux_runtime_paths(tmux).0;
        std::fs::create_dir_all(Path::new(&wrapper).parent().unwrap()).unwrap();
        std::fs::write(&wrapper, b"0123456").unwrap();
        install_source(tmux, &b, "session-b");
        let mut state = seeded_row(5_300_001, tmux, &seed);
        save_intake_inflight(Some(&seed), &mut state).unwrap();
        assert_eq!(
            saved(5_300_001),
            (
                Some(wrapper),
                7,
                Some(7),
                Some("intake-session".to_string())
            ),
            "a replaced source falls back to the wrapper end, never A's or B's rollout"
        );

        let tmux = "AgentDesk-codex-seed-kept";
        let a = rollout("kept.jsonl", 17);
        install_source(tmux, &a, "session-kept");
        let seed = observed_codex_tui_raw_seed(tmux).expect("observed seed of the kept source");
        assert_eq!((seed.path.as_str(), seed.end), (canonical(&a).as_str(), 17));
        std::fs::write(&a, vec![b'x'; 23]).unwrap();
        let mut state = seeded_row(5_300_002, tmux, &seed);
        save_intake_inflight(Some(&seed), &mut state).unwrap();
        assert_eq!(
            saved(5_300_002),
            (
                Some(canonical(&a)),
                17,
                Some(17),
                Some("intake-session".to_string())
            ),
            "the installed source keeps its seed although its rollout grew"
        );
    });
}

// The save waits for the row's sidecar before the source authority, the order terminal admission
// uses, so a terminal holding the sidecar can still take the authority.
#[test]
fn intake_save_takes_the_sidecar_before_the_source_authority() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (channel, tmux) = (5_300_003, "AgentDesk-codex-seed-lock-order");
    let seed = CodexRawSeed {
        tmux: tmux.to_string(),
        path: "/nonexistent/rollout.jsonl".to_string(),
        end: 0,
        session_id: "session-lock".to_string(),
    };
    let mut state = seeded_row(channel, tmux, &seed);
    let root = inflight::inflight_runtime_root().unwrap();
    let path = inflight::inflight_state_path(&root, &ProviderKind::Codex, channel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let (_latch, latch) = tokio::sync::oneshot::channel();
    let probe = inflight::SidecarAttemptProbe::new(root, latch);
    let sidecar = inflight::lock_inflight_state_path(&path).unwrap();
    let save = std::thread::spawn(move || save_intake_inflight(Some(&seed), &mut state));
    let attempts = || {
        probe
            .counts()
            .sources
            .get(&path)
            .map_or(0, |counts| counts[0])
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while attempts() < 2 {
        assert!(
            std::time::Instant::now() < deadline,
            "the intake save never reached the sidecar lock"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let contended = crate::services::tmux_common::source_authority_contention_key_for_tests(|| {
        crate::services::tmux_common::with_tmux_source_authority(tmux, |_| ())
    });
    drop(sidecar);
    let saved = save.join().expect("intake save thread");
    assert_eq!(
        contended, None,
        "the intake save held the source authority while it waited for the sidecar"
    );
    saved.unwrap();
}
