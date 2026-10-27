//! Classification 2 for the provider-isolation caller guards: as-written, comment-free
//! matching per caller file, so prose naming a helper is not a call. A lexical tripwire.

use crate::services::discord::router::source_guard_support::{Lexed, forge};
use crate::test_support::CommentForgery;

pub(super) const INTAKE_TURN: &str = include_str!("intake_turn.rs");
pub(super) const HEADLESS_TURN: &str = include_str!("headless_turn.rs");

const RAW_SEED: &str = "prelaunch_inflight_runtime_seed_without_codex_raw(";
const WATCHER_PATH: &str = "let watcher_output_path = prelaunch_watcher_output_path(";
const INTAKE_SAVE: &str = "save_intake_inflight(";
const DIRECT_SAVE: &str = "save_inflight_state_create_new(&inflight_state)";

/// Intake saves its row once, through the helper that rechecks a Codex raw seed's source.
pub(super) fn intake_save_gaps(intake: &str) -> Vec<String> {
    let lexed = Lexed::new(intake);
    let mut gaps = Vec::new();
    if lexed.count(INTAKE_SAVE) != 1 {
        gaps.push(format!("intake must save through {INTAKE_SAVE} once"));
    }
    if lexed.contains(DIRECT_SAVE) {
        gaps.push(format!(
            "intake bypasses the seed recheck with {DIRECT_SAVE}"
        ));
    }
    gaps
}

#[test]
fn intake_saves_through_the_seed_recheck() {
    assert_eq!(intake_save_gaps(INTAKE_TURN), Vec::<String>::new());
}

/// Headless prelaunch seeds without Codex raw evidence.
pub(super) fn raw_seed_gaps(headless: &str) -> Vec<String> {
    match Lexed::new(headless).contains(RAW_SEED) {
        true => Vec::new(),
        false => vec![format!("headless prelaunch lost {RAW_SEED}")],
    }
}

/// Each caller computes the wrapper watcher output path exactly once.
pub(super) fn watcher_path_gaps(callers: [&str; 2]) -> Vec<String> {
    callers
        .into_iter()
        .zip(["intake", "headless"])
        .filter(|(source, _)| Lexed::new(source).count(WATCHER_PATH) != 1)
        .map(|(_, caller)| format!("{caller} must compute the watcher path once"))
        .collect()
}

#[test]
fn caller_guards_reject_commented_calls() {
    for forgery in CommentForgery::ALL {
        let forged = forge(HEADLESS_TURN, RAW_SEED, forgery);
        assert!(forged.contains(RAW_SEED));
        assert_eq!(raw_seed_gaps(&forged).len(), 1, "{forgery:?}");
        let forged = forge(INTAKE_TURN, INTAKE_SAVE, forgery);
        assert!(forged.contains(INTAKE_SAVE));
        assert_eq!(intake_save_gaps(&forged).len(), 1, "{forgery:?}");
        for index in 0..2 {
            let mut callers = [INTAKE_TURN, HEADLESS_TURN];
            let forged = forge(callers[index], WATCHER_PATH, forgery);
            callers[index] = &forged;
            assert!(forged.contains(WATCHER_PATH));
            assert_eq!(watcher_path_gaps(callers).len(), 1, "{forgery:?} {index}");
        }
    }
}
