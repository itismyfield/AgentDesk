//! The reaper's automatic teardowns against stored rows: a found legacy row and, as in
//! main, a missing row go on; a Herdr, unreadable or marked session is left alone.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use futures::future::BoxFuture;
use poise::serenity_prelude::{ChannelId, MessageId};

use crate::services::discord::host_teardown_gate::test_support::{
    Stored, busy_turn, channel_key, runtime, seed,
};
use crate::services::discord::{DiscordSession, SharedData};
use crate::services::platform::tmux::PaneLiveness;
use crate::services::provider::ProviderKind;
use crate::services::tmux_diagnostics::PaneLivenessOverrideGuard;

/// Whether any tmux kill was requested for the session.
fn killed(name: &str) -> bool {
    crate::services::platform::tmux::kill_requests::count(name) > 0
}

fn own(name: &str) {
    let owner = crate::services::tmux_common::tmux_owner_path(name);
    std::fs::create_dir_all(std::path::Path::new(&owner).parent().unwrap()).unwrap();
    let marker = crate::services::tmux_common::current_tmux_owner_marker();
    std::fs::write(owner, marker).unwrap();
}

async fn map_channel(shared: &SharedData, channel: ChannelId, channel_name: &str) {
    let session = DiscordSession {
        session_id: None,
        memento_context_loaded: false,
        memento_reflected: false,
        current_path: None,
        history: Vec::new(),
        pending_uploads: Vec::new(),
        cleared: false,
        remote_profile_name: None,
        channel_id: Some(channel.get()),
        channel_name: Some(channel_name.to_string()),
        category_name: None,
        last_active: tokio::time::Instant::now(),
        worktree: None,
        born_generation: shared.restart.current_generation,
    };
    shared.core.lock().await.sessions.insert(channel, session);
}

fn admitted(stored: Stored) -> bool {
    matches!(stored, Stored::Legacy | Stored::Missing)
}

async fn postgres() -> (
    crate::db::auto_queue::test_support::TestPostgresDb,
    sqlx::PgPool,
) {
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    (db, pool)
}

// The stale-busy heal asks the host guard before its first probe, so a refused turn is
// neither probed nor finalized; a routine turn's missing row heals as in main.
#[tokio::test]
async fn stale_busy_heal_finalizes_only_a_turn_the_host_guard_admits_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (db, pool) = postgres().await;
    let (shared, _registry) = runtime(&pool).await;
    let mut turns = Vec::new();
    for (n, stored) in Stored::ALL.into_iter().enumerate() {
        let channel = ChannelId::new(1_479_671_301_387_060_000 + n as u64);
        let channel_name = format!("p4a-heal-{n}");
        let name = ProviderKind::Claude.build_tmux_session_name(&channel_name);
        map_channel(&shared, channel, &channel_name).await;
        seed(
            &pool,
            &channel_key(&shared, &name),
            &name,
            channel.get(),
            stored,
        )
        .await;
        busy_turn(&shared, channel, &name).await;
        turns.push((channel, name, stored));
    }
    shared
        .restart
        .global_active
        .store(turns.len(), Ordering::Relaxed);
    let probed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = probed.clone();
    let absent = move |name: String| -> BoxFuture<'static, bool> {
        seen.lock().unwrap().push(name);
        Box::pin(async { false })
    };
    let gate = super::host_guard::keyed_host_gate;
    super::reap_stale_busy_mailboxes_with_probe(&shared, &absent, &gate).await;

    let probed = probed.lock().unwrap().clone();
    for (channel, name, stored) in turns {
        let owner = crate::services::discord::mailbox_snapshot(&shared, channel).await;
        let released = owner.active_user_message_id != Some(MessageId::new(channel.get() + 1));
        assert_eq!(released, admitted(stored), "{stored:?}");
        assert_eq!(probed.contains(&name), admitted(stored), "{stored:?}");
    }
    pool.close().await;
    db.drop().await;
}

// The periodic dead-session pass: the guard reads the rows before the dispatch failure,
// the idle report and the kill; a failed pane probe is not death either.
#[tokio::test]
async fn dead_session_reaper_kills_only_what_the_host_guard_admits_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (db, pool) = postgres().await;
    let (shared, _registry) = runtime(&pool).await;
    let probe_error = (Stored::Legacy, PaneLiveness::ProbeError);
    let cases = Stored::ALL
        .into_iter()
        .map(|stored| (stored, PaneLiveness::DeadOrAbsent))
        .chain([probe_error]);
    let mut listed = Vec::new();
    let mut guards = Vec::new();
    for (n, (stored, pane)) in cases.enumerate() {
        let channel = ChannelId::new(1_479_671_301_387_060_100 + n as u64);
        let channel_name = format!("p4a-dead-{n}");
        let name = ProviderKind::Claude.build_tmux_session_name(&channel_name);
        map_channel(&shared, channel, &channel_name).await;
        seed(
            &pool,
            &channel_key(&shared, &name),
            &name,
            channel.get(),
            stored,
        )
        .await;
        own(&name);
        guards.push(PaneLivenessOverrideGuard::set(&name, pane));
        listed.push((
            name,
            admitted(stored) && pane == PaneLiveness::DeadOrAbsent,
            stored,
        ));
    }
    let names: Vec<String> = listed.iter().map(|(name, ..)| name.clone()).collect();
    super::reap_listed_dead_sessions(&shared, &names).await;
    for (name, expected, stored) in listed {
        assert_eq!(killed(&name), expected, "{stored:?} {name}");
    }
    pool.close().await;
    db.drop().await;
}

// Boot orphan cleanup has no channel for an orphan; its row and marker still refuse.
#[tokio::test]
async fn orphan_cleanup_kills_only_what_the_host_guard_admits_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (db, pool) = postgres().await;
    let (shared, _registry) = runtime(&pool).await;
    let mut listed = Vec::new();
    let mut guards = Vec::new();
    for (n, stored) in Stored::ALL.into_iter().enumerate() {
        let name = ProviderKind::Claude.build_tmux_session_name(&format!("p4a-orphan-{n}"));
        let channel = 1_479_671_301_387_060_200 + n as u64;
        seed(&pool, &channel_key(&shared, &name), &name, channel, stored).await;
        own(&name);
        guards.push(PaneLivenessOverrideGuard::set(
            &name,
            PaneLiveness::DeadOrAbsent,
        ));
        listed.push((name, stored));
    }
    let names: Vec<String> = listed.iter().map(|(name, _)| name.clone()).collect();
    super::clean_orphan_sessions(&shared, &names).await;
    for (name, stored) in listed {
        assert_eq!(killed(&name), admitted(stored), "{stored:?} {name}");
    }
    pool.close().await;
    db.drop().await;
}

/// A fresh routine with primary agent `a` and fallback agent `b`, and each one's session.
async fn fresh_routine(pool: &sqlx::PgPool, id: &str) -> (String, String) {
    sqlx::query(
        "INSERT INTO agents (id, name) VALUES ('a', 'a'), ('b', 'b') ON CONFLICT DO NOTHING",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO routines (id, agent_id, fallback_agent_id, script_ref, name, execution_strategy)
         VALUES ($1, 'a', 'b', 'script', $1, 'fresh')",
    )
    .bind(id)
    .execute(pool)
    .await
    .unwrap();
    let reread = crate::services::routines::fresh_session_reaper::reread_routine(pool, id);
    let routine = reread.await.unwrap().expect("the routine row");
    let name = |agent| {
        crate::services::routines::fresh_session_reaper::fresh_routine_owned_tmux_session_name(
            &routine,
            agent,
            &ProviderKind::Claude,
        )
    };
    (name("a"), name("b"))
}

async fn owned_run(pool: &sqlx::PgPool, run: &str, routine: &str, key: &str, age_secs: i64) {
    sqlx::query(
        "INSERT INTO routine_runs (id, routine_id, status, owned_tmux_session, started_at)
         VALUES ($1, $2, 'succeeded', $3, NOW() - make_interval(secs => $4))",
    )
    .bind(run)
    .bind(routine)
    .bind(key)
    .bind(age_secs as f64)
    .execute(pool)
    .await
    .unwrap();
}

// The listed-session pass reaps a fresh routine's dead session only on the row the run
// that owned that session recorded, even when a newer run owned the other agent's session.
#[tokio::test]
async fn fresh_routine_backstop_reads_the_run_that_owned_the_listed_session_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (db, pool) = postgres().await;
    let (shared, _registry) = runtime(&pool).await;
    // (primary's own run recorded, newer fallback run recorded, primary's rows, killed)
    let cases = [
        (true, true, Stored::Hosted, false),
        (true, true, Stored::Future, false),
        (true, true, Stored::LegacyHerdrMarker, false),
        (true, true, Stored::Legacy, true),
        (true, false, Stored::Hosted, false),
        (false, true, Stored::Missing, true),
        (false, false, Stored::MissingHerdrMarker, false),
    ];
    let mut listed = Vec::new();
    let mut guards = Vec::new();
    for (n, (primary_run, fallback_run, stored, expected)) in cases.into_iter().enumerate() {
        let routine = format!("p4a-routine-{n}");
        let (primary, fallback) = fresh_routine(&pool, &routine).await;
        // A routine session's row key is not the channel-style key for its tmux name.
        let key = |name: &str| format!("claude/routine-token/mac-mini:{name}");
        let channel = 1_479_671_301_387_060_300 + n as u64;
        let stored_key = if primary_run {
            key(&primary)
        } else {
            channel_key(&shared, &primary)
        };
        seed(&pool, &stored_key, &primary, channel, stored).await;
        if primary_run {
            owned_run(&pool, &format!("{routine}-a"), &routine, &key(&primary), 60).await;
        }
        if fallback_run {
            owned_run(&pool, &format!("{routine}-b"), &routine, &key(&fallback), 0).await;
        }
        own(&primary);
        let pane = PaneLiveness::DeadOrAbsent;
        guards.push(PaneLivenessOverrideGuard::set(&primary, pane));
        listed.push((primary, expected, stored));
    }
    let names: Vec<String> = listed.iter().map(|(name, ..)| name.clone()).collect();
    super::reap_listed_dead_sessions(&shared, &names).await;
    for (name, expected, stored) in listed {
        assert_eq!(killed(&name), expected, "{stored:?} {name}");
    }
    pool.close().await;
    db.drop().await;
}

// An unreadable ownership record keeps the fresh routine's session; it is not an orphan.
#[tokio::test]
async fn fresh_routine_backstop_keeps_the_session_when_ownership_is_unreadable_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (db, pool) = postgres().await;
    let (shared, _registry) = runtime(&pool).await;
    let (primary, _) = fresh_routine(&pool, "p4a-routine-unreadable").await;
    sqlx::query("ALTER TABLE routine_runs RENAME COLUMN owned_tmux_session TO p4a_unreadable")
        .execute(&pool)
        .await
        .unwrap();
    own(&primary);
    let _pane = PaneLivenessOverrideGuard::set(&primary, PaneLiveness::DeadOrAbsent);
    super::reap_listed_dead_sessions(&shared, std::slice::from_ref(&primary)).await;
    assert!(!killed(&primary), "{primary}");
    pool.close().await;
    db.drop().await;
}

// A completed unified-thread run's kill signal keys the thread channel's row.
#[tokio::test]
async fn unified_thread_kill_signal_kills_only_what_the_host_guard_admits_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (db, pool) = postgres().await;
    let (shared, _registry) = runtime(&pool).await;
    for (n, stored) in Stored::ALL.into_iter().enumerate() {
        let thread = 1_479_671_301_387_060_400 + n as u64;
        let name = ProviderKind::Claude.build_tmux_session_name(&format!("p4a-uni-t{thread}"));
        assert!(name.ends_with(&format!("-t{thread}")), "{name}");
        seed(&pool, &channel_key(&shared, &name), &name, thread, stored).await;
        let (names, thread) = (vec![name.clone()], thread.to_string());
        let target = super::kill_unified_thread_session(&shared, &thread, &names);
        assert_eq!(target.await.is_some(), admitted(stored), "{stored:?}");
        assert_eq!(killed(&name), admitted(stored), "{stored:?}");
    }
    pool.close().await;
    db.drop().await;
}

// Bound legacy orphans wait with unknown targets; `.channel` is not owner agreement.
#[tokio::test]
async fn c2b_orphan_cleanup_leaves_protected_and_unplaced_sessions_pg() {
    if !n3bc_isolated("c2b_orphan_cleanup_leaves_protected_and_unplaced_sessions_pg") {
        return;
    }
    use crate::services::discord::input_runtime::fence::{Gate, HoldUnknownForTest, test_health};
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (db, pool) = postgres().await;
    let (shared, _registry) = runtime(&pool).await;
    N3BC_ATTEMPTS.with_borrow_mut(std::collections::HashMap::clear);
    let base = 6_325_574_000u64;
    let gate = Gate::protect(ProviderKind::Claude, base + 1).unwrap();
    let _health = test_health::Clear::new(&gate);
    let _hold = HoldUnknownForTest::new();
    let mut guards = Vec::new();
    for (channel, bound) in [(base + 1, true), (base + 2, true), (base + 3, false)] {
        let name = ProviderKind::Claude.build_tmux_session_name(&format!("c2b2a-orphan-{channel}"));
        seed(
            &pool,
            &channel_key(&shared, &name),
            &name,
            channel,
            Stored::Legacy,
        )
        .await;
        own(&name);
        if bound {
            crate::services::tmux_common::write_tmux_channel_binding(&name, channel).unwrap();
        }
        guards.push(PaneLivenessOverrideGuard::set(
            &name,
            PaneLiveness::DeadOrAbsent,
        ));
        let file = crate::services::tmux_common::session_temp_path(&name, "jsonl");
        std::fs::write(&file, b"bound orphan original\n").unwrap();
        super::clean_orphan_sessions(&shared, std::slice::from_ref(&name)).await;
        assert!(!killed(&name), "a bound orphan is still Unknown: {name}");
        assert!(
            std::path::Path::new(&file).exists(),
            "held orphan file survives: {name}"
        );
    }
    assert_eq!(
        attempts("orphan_scan_start"),
        0,
        "helper holds before scan starts"
    );
    assert_eq!(
        attempts("orphan_host_start"),
        0,
        "Unknown guard avoids host teardown work"
    );
    pool.close().await;
    db.drop().await;
}

/// The periodic dead-session reaper leaves a protected channel's mapped dead session, as
/// boot restore leaves its CoreState entry for the move or handback.
#[tokio::test]
async fn c2b_dead_session_reaper_leaves_a_protected_channel_pg() {
    use crate::services::discord::input_runtime::fence::{Gate, test_health};
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (db, pool) = postgres().await;
    let (shared, _registry) = runtime(&pool).await;
    let (protected, legacy) = (6_325_574_101u64, 6_325_574_102u64);
    let gate = Gate::protect(ProviderKind::Claude, protected).unwrap();
    let _health = test_health::Clear::new(&gate);
    let mut listed = Vec::new();
    let mut guards = Vec::new();
    for channel in [protected, legacy] {
        let channel_name = format!("c2b2a-dead-{channel}");
        let name = ProviderKind::Claude.build_tmux_session_name(&channel_name);
        map_channel(&shared, ChannelId::new(channel), &channel_name).await;
        seed(
            &pool,
            &channel_key(&shared, &name),
            &name,
            channel,
            Stored::Legacy,
        )
        .await;
        own(&name);
        guards.push(PaneLivenessOverrideGuard::set(
            &name,
            PaneLiveness::DeadOrAbsent,
        ));
        listed.push((name, channel == legacy));
    }
    let names: Vec<String> = listed.iter().map(|(name, _)| name.clone()).collect();
    super::reap_listed_dead_sessions(&shared, &names).await;
    for (name, expected) in listed {
        assert_eq!(killed(&name), expected, "{name}");
    }
    pool.close().await;
    db.drop().await;
}

fn n3bc_isolated(name: &str) -> bool {
    const CHILD: &str = "ADK_N3BC_REAPER_CHILD";
    if std::env::var_os(CHILD).is_some() {
        return true;
    }
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let script = r#"#!/bin/sh
printf '%s\n' "$*" >> "ROOT/tmux-calls"
while [ "${1#-}" != "$1" ]; do
  case "$1" in -V) echo 'tmux fixture'; exit 0 ;; -L|-S) shift ;; esac
  shift
done
case "$1" in
  list-sessions) /bin/cat "ROOT/tmux-sessions" ;;
  has-session) exit 0 ;;
  list-panes) echo 1 ;;
  show-option|show-options) exit 0 ;;
  kill-session) printf '%s\n' "$*" >> "ROOT/tmux-kills" ;;
  *) printf '%s\n' "$*" >> "ROOT/tmux-errors"; exit 97 ;;
esac
"#;
    for (name, script) in [
        ("tmux", script.replace("ROOT", &root.display().to_string())),
        ("ps", "#!/bin/sh\nexit 0\n".into()),
    ] {
        let file = root.join(name);
        std::fs::write(&file, script).unwrap();
        std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    std::fs::write(root.join("tmux-sessions"), []).unwrap();
    let exact = format!("{}::{name}", module_path!().split_once("::").unwrap().1);
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &exact, "--nocapture"])
        .env(CHILD, "1")
        .env_remove("DATABASE_URL")
        .env("AGENTDESK_ROOT_DIR", root)
        .env("TMPDIR", root)
        .env("CODEX_HOME", root.join("codex"))
        .env("CLAUDE_CONFIG_DIR", root.join("claude"))
        .env("PATH", format!("{}:/usr/bin:/bin", root.display()))
        .env("HTTP_PROXY", "http://127.0.0.1:9")
        .env("HTTPS_PROXY", "http://127.0.0.1:9")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env(
            crate::services::tui_o::cutover::test_override::CHILD_ENV,
            "1",
        )
        .env(
            crate::services::tui_o::cutover::test_override::CHANNELS_ENV,
            "[]",
        )
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{exact}:\n{stdout}\n{stderr}");
    assert!(stdout.contains("1 passed; 0 failed; 0 ignored"), "{stdout}");
    let errors = std::fs::read_to_string(root.join("tmux-errors")).unwrap_or_default();
    assert!(errors.is_empty(), "unexpected fake command: {errors}");
    false
}

fn n3bc_root() -> std::path::PathBuf {
    std::env::var_os("AGENTDESK_ROOT_DIR").unwrap().into()
}

async fn n3bc_mode(
    shared: &SharedData,
    channel: u64,
    mode: crate::services::discord::input_runtime::fence::Mode,
) -> Arc<crate::services::discord::input_runtime::fence::Gate> {
    use crate::services::discord::input_runtime::fence::{Gate, Mode};
    let gate = Gate::protect(ProviderKind::Claude, channel).unwrap();
    if mode != Mode::LegacyOpen {
        let closing = Arc::new(gate.close().unwrap());
        if mode != Mode::Closing {
            let _transition = shared
                .freeze_legacy_queue(
                    closing.clone(),
                    crate::services::turn_orchestrator::QueuePersistenceContext::new(
                        &ProviderKind::Claude,
                        "n3bc",
                        None,
                    ),
                )
                .await
                .unwrap();
            match mode {
                Mode::LedgerOpen => closing.open_ledger().unwrap(),
                Mode::Handback => closing.begin_handback().unwrap(),
                Mode::Held => closing.hold().unwrap(),
                Mode::Frozen => {}
                _ => unreachable!(),
            }
        }
    }
    assert_eq!(gate.mode(), mode);
    gate
}

const N3BC_MODES: [crate::services::discord::input_runtime::fence::Mode; 6] = {
    use crate::services::discord::input_runtime::fence::Mode;
    [
        Mode::LegacyOpen,
        Mode::Closing,
        Mode::Frozen,
        Mode::LedgerOpen,
        Mode::Handback,
        Mode::Held,
    ]
};

#[tokio::test]
async fn n3bc_public_orphan_conflicting_channel_is_unknown_pg() {
    if !n3bc_isolated("n3bc_public_orphan_conflicting_channel_is_unknown_pg") {
        return;
    }
    use crate::services::discord::input_runtime::fence::{HoldUnknownForTest, test_health};
    let (db, pool) = postgres().await;
    let (shared, _registry) = runtime(&pool).await;
    for (n, mode) in N3BC_MODES.into_iter().enumerate() {
        let protected = 6_325_574_400 + n as u64;
        let legacy = protected + 100;
        let gate = n3bc_mode(&shared, protected, mode).await;
        let _health = test_health::Clear::new(&gate);
        let name = ProviderKind::Claude.build_tmux_session_name(&format!("n3bc-conflict-{legacy}"));
        // The row names P while the marker names L: a marker alone cannot settle identity.
        seed(
            &pool,
            &channel_key(&shared, &name),
            &name,
            protected,
            Stored::Legacy,
        )
        .await;
        n3bc_dispatch(
            &pool,
            &channel_key(&shared, &name),
            &format!("n3bc-conflict-dispatch-{n}"),
        )
        .await;
        own(&name);
        crate::services::tmux_common::write_tmux_channel_binding(&name, legacy).unwrap();
        let file = crate::services::tmux_common::session_temp_path(&name, "jsonl");
        std::fs::write(&file, b"original\n").unwrap();
        let before = std::fs::metadata(&file).unwrap().modified().unwrap();
        std::fs::write(n3bc_root().join("tmux-sessions"), &name).unwrap();
        let _hold = HoldUnknownForTest::new();
        super::cleanup_orphan_tmux_sessions(&shared).await;
        assert!(
            std::fs::read_to_string(n3bc_root().join("tmux-calls"))
                .unwrap_or_default()
                .is_empty(),
            "public hold avoids tmux list"
        );
        assert!(
            !killed(&name),
            "conflicting public orphan preserved in {mode:?}"
        );
        assert_eq!(
            attempts("orphan_fail_start"),
            0,
            "protected dispatch failure is never started"
        );
        assert!(
            std::path::Path::new(&file).exists(),
            "protected conflicting orphan file survives"
        );
        assert_eq!(std::fs::read(&file).unwrap(), b"original\n");
        assert_eq!(
            std::fs::metadata(&file).unwrap().modified().unwrap(),
            before
        );
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn n3bc_public_periodic_fresh_hold_keeps_mapped_sibling_pg() {
    if !n3bc_isolated("n3bc_public_periodic_fresh_hold_keeps_mapped_sibling_pg") {
        return;
    }
    use crate::services::discord::input_runtime::fence::{HoldUnknownForTest, test_health};
    let (db, pool) = postgres().await;
    let (shared, _registry) = runtime(&pool).await;
    for (n, mode) in N3BC_MODES.into_iter().enumerate() {
        let protected = 6_325_574_600 + n as u64;
        let gate = n3bc_mode(&shared, protected, mode).await;
        let _health = test_health::Clear::new(&gate);
        let (orphan, _) = fresh_routine(&pool, &format!("n3bc-fresh-{n}")).await;
        seed(
            &pool,
            &channel_key(&shared, &orphan),
            &orphan,
            protected + 100,
            Stored::Legacy,
        )
        .await;
        own(&orphan);
        crate::services::tmux_common::write_tmux_channel_binding(&orphan, protected + 100).unwrap();
        let file = crate::services::tmux_common::session_temp_path(&orphan, "jsonl");
        std::fs::write(&file, b"fresh original\n").unwrap();
        let legacy = protected + 200;
        let channel_name = format!("n3bc-sibling-{legacy}");
        let sibling = ProviderKind::Claude.build_tmux_session_name(&channel_name);
        map_channel(&shared, ChannelId::new(legacy), &channel_name).await;
        seed(
            &pool,
            &channel_key(&shared, &sibling),
            &sibling,
            legacy,
            Stored::Legacy,
        )
        .await;
        own(&sibling);
        std::fs::write(
            n3bc_root().join("tmux-sessions"),
            format!("{orphan}\n{sibling}\n"),
        )
        .unwrap();
        let _hold = HoldUnknownForTest::new();
        N3BC_ATTEMPTS.with_borrow_mut(std::collections::HashMap::clear);
        super::reap_dead_tmux_sessions(&shared).await;
        assert!(
            !killed(&orphan),
            "Fresh Unknown survives public periodic {mode:?}"
        );
        assert!(
            std::path::Path::new(&file).exists(),
            "protected Fresh file survives in {mode:?}"
        );
        assert_eq!(
            attempts("fresh_reread_start"),
            0,
            "protected Fresh orphan does not start reread"
        );
        assert_eq!(std::fs::read(&file).unwrap(), b"fresh original\n");
        assert!(
            killed(&sibling),
            "mapped Legacy sibling is cleaned in {mode:?}"
        );
    }
    pool.close().await;
    db.drop().await;
}

#[derive(Default)]
struct N3bcPause {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
thread_local! {
    static N3BC_PAUSE: std::cell::RefCell<Option<(&'static str, String, Arc<N3bcPause>)>> = const { std::cell::RefCell::new(None) };
    static N3BC_ATTEMPTS: std::cell::RefCell<std::collections::HashMap<&'static str, usize>> = std::cell::RefCell::new(std::collections::HashMap::new());
}
pub(super) async fn boundary(stage: &'static str, name: &str) {
    N3BC_ATTEMPTS.with_borrow_mut(|attempts| *attempts.entry(stage).or_default() += 1);
    let pause = N3BC_PAUSE.with_borrow(|pause| {
        pause
            .as_ref()
            .filter(|(target, session, _)| *target == stage && session == name)
            .map(|(_, _, pause)| pause.clone())
    });
    if let Some(pause) = pause {
        pause.entered.notify_one();
        pause.release.notified().await;
    }
}
fn attempts(stage: &'static str) -> usize {
    N3BC_ATTEMPTS.with_borrow(|counts| counts.get(stage).copied().unwrap_or(0))
}

#[tokio::test]
async fn n3bc_orphan_post_await_boundaries_preserve_after_install_pg() {
    if !n3bc_isolated("n3bc_orphan_post_await_boundaries_preserve_after_install_pg") {
        return;
    }
    use crate::services::discord::input_runtime::fence::{Gate, HoldUnknownForTest, test_health};
    let (db, pool) = postgres().await;
    let (shared, _registry) = runtime(&pool).await;
    for (n, stage) in [
        "orphan_scan_start",
        "orphan_scan",
        "orphan_host",
        "orphan_pane",
        "orphan_before_kill",
        "orphan_kill",
    ]
    .into_iter()
    .enumerate()
    {
        let channel = 6_325_574_800 + n as u64;
        let name =
            ProviderKind::Claude.build_tmux_session_name(&format!("n3bc-orphan-await-{channel}"));
        seed(
            &pool,
            &channel_key(&shared, &name),
            &name,
            channel,
            Stored::Legacy,
        )
        .await;
        own(&name);
        crate::services::tmux_common::write_tmux_channel_binding(&name, channel).unwrap();
        let file = crate::services::tmux_common::session_temp_path(&name, "jsonl");
        std::fs::write(&file, b"boundary original\n").unwrap();
        std::fs::write(n3bc_root().join("tmux-sessions"), &name).unwrap();
        N3BC_ATTEMPTS.with_borrow_mut(std::collections::HashMap::clear);
        let pause = Arc::new(N3bcPause::default());
        let pause_name = if stage == "orphan_scan_start" {
            String::new()
        } else {
            name.clone()
        };
        N3BC_PAUSE.with_borrow_mut(|p| *p = Some((stage, pause_name, pause.clone())));
        let work = super::cleanup_orphan_tmux_sessions(&shared);
        tokio::pin!(work);
        tokio::select! {
            _ = pause.entered.notified() => {},
            _ = &mut work => panic!("orphan failed to reach {stage}"),
        }
        let gate = Gate::protect(ProviderKind::Claude, channel + 100).unwrap();
        let _health = test_health::Clear::new(&gate);
        let _hold = HoldUnknownForTest::new();
        let kills = crate::services::platform::tmux::kill_requests::count(&name);
        let before = (
            std::fs::read(&file).unwrap(),
            std::fs::metadata(&file).unwrap().modified().unwrap(),
        );
        pause.release.notify_one();
        work.await;
        N3BC_PAUSE.with_borrow_mut(|p| *p = None);
        assert_eq!(
            crate::services::platform::tmux::kill_requests::count(&name),
            kills,
            "no kill after {stage} protection"
        );
        assert!(
            std::path::Path::new(&file).exists(),
            "file survives protected {stage}"
        );
        assert_eq!(
            std::fs::read(&file).unwrap(),
            before.0,
            "no unlink after {stage} protection"
        );
        assert_eq!(
            std::fs::metadata(&file).unwrap().modified().unwrap(),
            before.1
        );
        if matches!(stage, "orphan_scan_start" | "orphan_scan") {
            assert_eq!(
                attempts("orphan_host_start"),
                0,
                "protected scan avoids host work"
            );
        }
        if stage == "orphan_host" {
            assert_eq!(
                attempts("orphan_pane_start"),
                0,
                "no pane work after protected host await"
            );
        }
        if stage == "orphan_pane" {
            assert_eq!(
                attempts("orphan_dispatch"),
                0,
                "no dispatch work after protected pane await"
            );
        }
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn n3bc_fresh_post_await_boundaries_preserve_after_install_pg() {
    if !n3bc_isolated("n3bc_fresh_post_await_boundaries_preserve_after_install_pg") {
        return;
    }
    use crate::services::discord::input_runtime::fence::{Gate, HoldUnknownForTest, test_health};
    let (db, pool) = postgres().await;
    let (shared, _registry) = runtime(&pool).await;
    for (n, stage) in ["fresh_reread", "fresh_host", "fresh_pane", "fresh_kill"]
        .into_iter()
        .enumerate()
    {
        let channel = 6_325_574_900 + n as u64;
        let (name, _) = fresh_routine(&pool, &format!("n3bc-fresh-await-{channel}")).await;
        seed(
            &pool,
            &channel_key(&shared, &name),
            &name,
            channel,
            Stored::Legacy,
        )
        .await;
        own(&name);
        let file = crate::services::tmux_common::session_temp_path(&name, "jsonl");
        std::fs::write(&file, b"fresh boundary original\n").unwrap();
        std::fs::write(n3bc_root().join("tmux-sessions"), &name).unwrap();
        N3BC_ATTEMPTS.with_borrow_mut(std::collections::HashMap::clear);
        let pause = Arc::new(N3bcPause::default());
        N3BC_PAUSE.with_borrow_mut(|p| *p = Some((stage, name.clone(), pause.clone())));
        let work = super::reap_dead_tmux_sessions(&shared);
        tokio::pin!(work);
        tokio::select! {
            _ = pause.entered.notified() => {},
            _ = &mut work => panic!("Fresh orphan failed to reach {stage}"),
        }
        let gate = Gate::protect(ProviderKind::Claude, channel + 100).unwrap();
        let _health = test_health::Clear::new(&gate);
        let _hold = HoldUnknownForTest::new();
        let kills = crate::services::platform::tmux::kill_requests::count(&name);
        let before = (
            std::fs::read(&file).unwrap(),
            std::fs::metadata(&file).unwrap().modified().unwrap(),
        );
        pause.release.notify_one();
        work.await;
        N3BC_PAUSE.with_borrow_mut(|p| *p = None);
        assert_eq!(
            crate::services::platform::tmux::kill_requests::count(&name),
            kills,
            "no Fresh kill after {stage} protection"
        );
        assert!(
            std::path::Path::new(&file).exists(),
            "file survives protected {stage}"
        );
        assert_eq!(
            std::fs::read(&file).unwrap(),
            before.0,
            "no Fresh unlink after {stage} protection"
        );
        assert_eq!(
            std::fs::metadata(&file).unwrap().modified().unwrap(),
            before.1
        );
        if stage == "fresh_reread" {
            assert_eq!(
                attempts("fresh_host_start"),
                0,
                "no host work after protected reread"
            );
        }
        if stage == "fresh_pane" {
            assert_eq!(
                attempts("fresh_before_kill"),
                0,
                "protected Fresh pane avoids kill work"
            );
        }
        if stage == "fresh_host" {
            assert_eq!(
                attempts("fresh_pane_start"),
                0,
                "no pane work after protected Fresh host await"
            );
        }
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn n3bc_mapped_dead_post_await_preserves_after_install_pg() {
    if !n3bc_isolated("n3bc_mapped_dead_post_await_preserves_after_install_pg") {
        return;
    }
    use crate::services::discord::input_runtime::fence::{Gate, test_health};
    let (db, pool) = postgres().await;
    let (shared, _registry) = runtime(&pool).await;
    for (n, stage) in ["mapped_host", "mapped_row"].into_iter().enumerate() {
        let channel = 6_325_575_800 + n as u64;
        let channel_name = format!("n3bc-mapped-await-{channel}");
        let name = ProviderKind::Claude.build_tmux_session_name(&channel_name);
        map_channel(&shared, ChannelId::new(channel), &channel_name).await;
        seed(
            &pool,
            &channel_key(&shared, &name),
            &name,
            channel,
            Stored::Legacy,
        )
        .await;
        own(&name);
        std::fs::write(n3bc_root().join("tmux-sessions"), &name).unwrap();
        N3BC_ATTEMPTS.with_borrow_mut(std::collections::HashMap::clear);
        let pause = Arc::new(N3bcPause::default());
        N3BC_PAUSE.with_borrow_mut(|p| *p = Some((stage, name.clone(), pause.clone())));
        let work = super::reap_dead_tmux_sessions(&shared);
        tokio::pin!(work);
        tokio::select! {
            _ = pause.entered.notified() => {},
            _ = &mut work => panic!("mapped dead session failed to reach {stage}"),
        }
        let gate = Gate::protect(ProviderKind::Claude, channel).unwrap();
        let _health = test_health::Clear::new(&gate);
        let kills = crate::services::platform::tmux::kill_requests::count(&name);
        pause.release.notify_one();
        work.await;
        N3BC_PAUSE.with_borrow_mut(|p| *p = None);
        assert_eq!(
            crate::services::platform::tmux::kill_requests::count(&name),
            kills,
            "mapped Legacy kill held after {stage}"
        );
        if stage == "mapped_host" {
            assert_eq!(
                attempts("mapped_dispatch"),
                0,
                "post-host protection avoids dispatch work"
            );
        }
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn n3bc_last_protection_release_allows_existing_orphan_cleanup_pg() {
    if !n3bc_isolated("n3bc_last_protection_release_allows_existing_orphan_cleanup_pg") {
        return;
    }
    use crate::services::discord::input_runtime::{
        fence::{Gate, HoldUnknownForTest, any_protected, test_health},
        supervisor::ordering::AdmissionOrder,
    };
    assert!(!any_protected(), "fresh child starts with registry zero");
    let (db, pool) = postgres().await;
    let (shared, _registry) = runtime(&pool).await;
    let channel = 6_325_575_900;
    let name = ProviderKind::Claude.build_tmux_session_name("n3bc-last-release");
    seed(
        &pool,
        &channel_key(&shared, &name),
        &name,
        channel + 1,
        Stored::Legacy,
    )
    .await;
    own(&name);
    crate::services::tmux_common::write_tmux_channel_binding(&name, channel + 1).unwrap();
    let file = crate::services::tmux_common::session_temp_path(&name, "jsonl");
    std::fs::write(&file, b"release fixture\n").unwrap();
    std::fs::write(n3bc_root().join("tmux-sessions"), &name).unwrap();
    let gate = Gate::protect(ProviderKind::Claude, channel).unwrap();
    let _health = test_health::Clear::new(&gate);
    {
        let _hold = HoldUnknownForTest::new();
        super::cleanup_orphan_tmux_sessions(&shared).await;
    }
    assert!(!killed(&name));
    assert!(std::path::Path::new(&file).exists());
    assert!(
        any_protected(),
        "TLS hold release did not release registry protection"
    );
    let closing = Arc::new(gate.close().unwrap());
    let _transition = shared
        .freeze_legacy_queue(
            closing.clone(),
            crate::services::turn_orchestrator::QueuePersistenceContext::new(
                &ProviderKind::Claude,
                "n3bc-release",
                None,
            ),
        )
        .await
        .unwrap();
    closing.begin_handback().unwrap();
    let pending = AdmissionOrder::new(ProviderKind::Claude, channel, 7, 0)
        .expect("fresh admission order")
        .pending_snapshot();
    closing
        .release_after_handback_with_pending(&pending)
        .unwrap();
    assert!(!any_protected(), "last protection was actually released");
    super::cleanup_orphan_tmux_sessions(&shared).await;
    assert!(killed(&name), "a later existing cleanup invocation resumes");
    assert!(
        !std::path::Path::new(&file).exists(),
        "resumed cleanup removes old files"
    );
    pool.close().await;
    db.drop().await;
}

async fn n3bc_dispatch(pool: &sqlx::PgPool, key: &str, dispatch: &str) {
    sqlx::query("INSERT INTO task_dispatches (id, status) VALUES ($1, 'dispatched')")
        .bind(dispatch)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("UPDATE sessions SET status = 'turn_active', active_dispatch_id = $1 WHERE session_key = $2")
        .bind(dispatch).bind(key).execute(pool).await.unwrap();
}

#[tokio::test]
async fn n3bc_orphan_dispatch_boundary_preserves_after_install_pg() {
    if !n3bc_isolated("n3bc_orphan_dispatch_boundary_preserves_after_install_pg") {
        return;
    }
    use crate::services::discord::input_runtime::fence::{Gate, HoldUnknownForTest, test_health};
    let (db, pool) = postgres().await;
    let (shared, _registry) = runtime(&pool).await;
    let channel = 6_325_575_950;
    let name = ProviderKind::Claude.build_tmux_session_name("n3bc-orphan-dispatch");
    let key = channel_key(&shared, &name);
    seed(&pool, &key, &name, channel, Stored::Legacy).await;
    n3bc_dispatch(&pool, &key, "n3bc-orphan-dispatch-id").await;
    own(&name);
    let file = crate::services::tmux_common::session_temp_path(&name, "jsonl");
    std::fs::write(&file, b"dispatch original\n").unwrap();
    std::fs::write(n3bc_root().join("tmux-sessions"), &name).unwrap();
    let pause = Arc::new(N3bcPause::default());
    N3BC_ATTEMPTS.with_borrow_mut(std::collections::HashMap::clear);
    N3BC_PAUSE.with_borrow_mut(|p| *p = Some(("orphan_fail", name.clone(), pause.clone())));
    let work = super::cleanup_orphan_tmux_sessions(&shared);
    tokio::pin!(work);
    tokio::select! {
        _ = pause.entered.notified() => {},
        _ = &mut work => panic!("active dispatch did not reach failure boundary"),
    }
    let gate = Gate::protect(ProviderKind::Claude, channel + 1).unwrap();
    let _health = test_health::Clear::new(&gate);
    let _hold = HoldUnknownForTest::new();
    pause.release.notify_one();
    work.await;
    N3BC_PAUSE.with_borrow_mut(|p| *p = None);
    assert_eq!(
        attempts("orphan_fail_start"),
        0,
        "no dispatch failure after protection"
    );
    assert!(
        !killed(&name),
        "no orphan kill after dispatch boundary protection"
    );
    assert!(
        std::path::Path::new(&file).exists(),
        "protected dispatch orphan file survives"
    );
    assert_eq!(std::fs::read(&file).unwrap(), b"dispatch original\n");
    let state: (String, Option<String>) =
        sqlx::query_as("SELECT status, active_dispatch_id FROM sessions WHERE session_key = $1")
            .bind(&key)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        state,
        ("turn_active".into(), Some("n3bc-orphan-dispatch-id".into()))
    );
    pool.close().await;
    db.drop().await;
}
