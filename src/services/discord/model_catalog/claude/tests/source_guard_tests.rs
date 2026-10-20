//! Classification 2: the refresh-state lock guard reads the locked scope item and the test
//! module with comments excluded. A lexical tripwire over this file's tests only.

use crate::services::discord::router::source_guard_support::{Lexed, forge, swap};
use crate::test_support::CommentForgery;

pub(super) const CLAUDE: &str = include_str!("../../claude.rs");

const SCOPE: &str = "pub(super) fn with_test_refresh_task_state";
const LOCK: &str = "acquire_shared_test_env_lock";
const REFRESH_STATE_STORE: &str = concat!("REFRESH_TASK_RUNNING", ".store");
const TEST_MODULE: &str = "#[cfg(test)]\nmod tests";

/// The canonical lock is taken before the scoped reset, and from the test module to EOF
/// no code stores refresh state directly.
pub(super) fn refresh_state_lock_gaps(source: &str) -> Vec<String> {
    let lexed = Lexed::new(source);
    let mut gaps = Vec::new();
    match lexed.block(SCOPE) {
        Some(scope) => match (scope.find(LOCK), scope.find(REFRESH_STATE_STORE)) {
            (Some(lock), Some(reset)) if lock < reset => {}
            found => gaps.push(format!(
                "the canonical lock must precede the reset: {found:?}"
            )),
        },
        None => gaps.push(format!("missing {SCOPE}")),
    }
    match lexed.find(TEST_MODULE) {
        Some(at) if lexed.text()[at..].contains(REFRESH_STATE_STORE) => {
            gaps.push("tests must mutate refresh state only through the locked scope".to_string())
        }
        Some(_) => {}
        None => gaps.push(format!("missing {TEST_MODULE}")),
    }
    gaps
}

#[test]
fn refresh_state_guard_rejects_commented_lock_and_counts_coded_stores() {
    for needle in [SCOPE, LOCK, REFRESH_STATE_STORE, TEST_MODULE] {
        for forgery in CommentForgery::ALL {
            let forged = forge(CLAUDE, needle, forgery);
            assert!(
                !refresh_state_lock_gaps(&forged).is_empty(),
                "{forgery:?} commented `{needle}` still passes"
            );
        }
    }
    let late_lock = swap(
        CLAUDE,
        "let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();",
        "REFRESH_TASK_RUNNING.store(false, Ordering::Release);",
    );
    assert!(!refresh_state_lock_gaps(&late_lock).is_empty());
    let test_fn = "fn invariant_claude_model_catalog_refresh_claim_is_singleflight() {";
    let store = format!("{REFRESH_STATE_STORE}(true, Ordering::Release);");
    let prose = CLAUDE.replacen(test_fn, &format!("{test_fn}\n// {store}\n/* {store} */"), 1);
    assert_ne!(prose, CLAUDE);
    assert_eq!(refresh_state_lock_gaps(&prose), Vec::<String>::new());
    let coded = CLAUDE.replacen(test_fn, &format!("{test_fn}\n{store}"), 1);
    assert_eq!(refresh_state_lock_gaps(&coded).len(), 1);
}
