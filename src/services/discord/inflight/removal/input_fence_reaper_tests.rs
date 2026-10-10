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

/// Episode directories under the boot custody root, by the channel each marker names.
fn custody_channels(env: &Env) -> Vec<u64> {
    let root = env.dir().with_file_name("discord_custody").join("claude");
    let dirs = fs::read_dir(root).into_iter().flatten().flatten();
    let mut channels: Vec<u64> = dirs
        .filter_map(|dir| {
            let marker = fs::read(dir.path().join("episode.json")).ok()?;
            let marker: serde_json::Value = serde_json::from_slice(&marker).ok()?;
            marker["episode"]["channel_id"].as_u64()
        })
        .collect();
    channels.sort_unstable();
    channels
}

/// Boot preparation settles before a caller may reserve the provider's pass: a caller held
/// in preparation neither copies nor unlinks, and a later-prepared caller runs the pass.
#[tokio::test]
async fn c2b_boot_preparation_settles_before_the_reaper_pass_is_reserved() {
    let env = Env::new();
    let legacy = 6_325_570;
    let legacy_path = env.seed(&row(legacy, None), STALE);
    let guard = BootReapOnce::default();
    let (release, latch) = tokio::sync::oneshot::channel::<()>();
    let held = reap_inflight_rows_after_preparation(&guard, &CLAUDE, None, async move {
        let _ = latch.await;
        BootPreparation::Unconfigured
    });
    tokio::pin!(held);
    for _ in 0..3 {
        assert!(futures::poll!(held.as_mut()).is_pending());
        tokio::task::yield_now().await;
    }
    assert!(legacy_path.exists(), "no unlink while preparation is held");
    assert!(custody_channels(&env).is_empty(), "no custody copy either");

    let prepared = reap_inflight_rows_at_boot_with_guard(&guard, &CLAUDE, None).await;
    assert!(
        !prepared.already_ran,
        "the held caller reserved no pass before preparing"
    );
    assert_eq!(prepared.reaped_stale, 1);
    assert_eq!(custody_channels(&env), vec![legacy]);

    release.send(()).unwrap();
    let late = held.await;
    assert!(late.already_ran, "the provider's pass ran once");
}

/// Boot custody leaves a protected channel's row and pending-start record unread, and still
/// copies an unprotected sibling's row before the reaper unlinks it.
#[tokio::test]
async fn c2b_boot_custody_leaves_a_protected_channel_unread() {
    let env = Env::new();
    let (protected, legacy) = (6_325_571, 6_325_572);
    let protected_path = env.seed(&row(protected, None), STALE);
    let legacy_path = env.seed(&row(legacy, None), STALE);
    let pending = crate::services::discord::runtime_store::tui_direct_pending_start_root();
    let pending = pending.unwrap();
    fs::create_dir_all(&pending).unwrap();
    let record = pending.join(format!("claude_{protected}_{}.json", protected + 1));
    fs::write(&record, b"{\"provider\":\"claude\"}").unwrap();
    let before = (
        fs::read(&protected_path).unwrap(),
        fs::read(&record).unwrap(),
    );
    let gate = Gate::protect(CLAUDE, protected).unwrap();
    let _health = input_runtime::fence::test_health::Clear::new(&gate);

    let guard = BootReapOnce::default();
    let report = reap_inflight_rows_at_boot_with_guard(&guard, &CLAUDE, None).await;

    assert_eq!(
        custody_channels(&env),
        vec![legacy],
        "only the sibling is copied"
    );
    assert_eq!(
        (
            fs::read(&protected_path).unwrap(),
            fs::read(&record).unwrap()
        ),
        before
    );
    assert!(!legacy_path.exists());
    assert_eq!((report.protected, report.reaped_stale), (1, 1));
}
