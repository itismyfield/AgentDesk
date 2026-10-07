//! The boot reaper leaves an input-protected channel's population for its move or handback.

use super::nondestructive_loader_tests::{CLAUDE, Env, STALE, row};
use super::*;
use crate::services::discord::input_runtime::{self, fence::Gate};

#[test]
fn c2_boot_reaper_leaves_input_protected_rows_untouched() {
    let env = Env::new();
    let (open, closed, legacy) = (6_325_560, 6_325_561, 6_325_562);
    let open_path = env.seed(&row(open, None), STALE);
    let closed_path = env.seed(&row(closed, None), STALE);
    let legacy_path = env.seed(&row(legacy, None), STALE);
    let (open_before, closed_before) = (
        fs::read(&open_path).unwrap(),
        fs::read(&closed_path).unwrap(),
    );
    let open_gate = Gate::protect(CLAUDE, open).unwrap();
    let _open_health = input_runtime::fence::test_health::Clear::new(&open_gate);
    let closed_gate = Gate::protect(CLAUDE, closed).unwrap();
    let _closed_health = input_runtime::fence::test_health::Clear::new(&closed_gate);
    let _closing = closed_gate.close().unwrap();

    let report = reap_inflight_rows_at_boot_in_root(&env.dir(), &CLAUDE);

    assert_eq!(report.protected, 2);
    assert_eq!(
        fs::read(&open_path).unwrap(),
        open_before,
        "LegacyOpen gate row kept"
    );
    assert_eq!(
        fs::read(&closed_path).unwrap(),
        closed_before,
        "Closing gate row kept"
    );
    assert!(
        !legacy_path.exists(),
        "an unprotected stale row is reaped as before"
    );
    assert_eq!(report.reaped_stale, 1);
    let tried = |channel: u64| {
        input_runtime::health_reasons()
            .iter()
            .any(|reason| reason.contains(&format!("channel={channel}")))
    };
    assert!(!tried(open) && !tried(closed), "no row lock was attempted");
}

#[test]
fn c2_stale_generation_invalidate_leaves_input_protected_rows_untouched() {
    let env = Env::new();
    let (protected, legacy) = (6_325_563, 6_325_564);
    let stamped = |channel: u64| {
        let mut state = row(channel, None);
        state.restart_generation = Some(super::nondestructive_loader_tests::G - 1);
        state
    };
    let protected_path = env.seed(&stamped(protected), 0);
    let legacy_path = env.seed(&stamped(legacy), 0);
    let before = fs::read(&protected_path).unwrap();
    let gate = Gate::protect(CLAUDE, protected).unwrap();
    let _health = input_runtime::fence::test_health::Clear::new(&gate);

    let removed = invalidate_stale_generation_in_root(
        &env.dir(),
        &CLAUDE,
        super::nondestructive_loader_tests::G,
    );

    assert_eq!(
        fs::read(&protected_path).unwrap(),
        before,
        "protected row kept"
    );
    assert!(
        !legacy_path.exists(),
        "an unprotected prior-generation row is removed as before"
    );
    assert_eq!(
        removed,
        vec![(legacy, Some(super::nondestructive_loader_tests::G - 1))]
    );
}
