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

/// Boot orphan cleanup leaves an unowned session that its `.channel` binding places on a
/// protected channel, and one no binding places while unknown targets are held.
#[tokio::test]
async fn c2b_orphan_cleanup_leaves_protected_and_unplaced_sessions_pg() {
    use crate::services::discord::input_runtime::fence::{Gate, HoldUnknownForTest, test_health};
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (db, pool) = postgres().await;
    let (shared, _registry) = runtime(&pool).await;
    let base = 6_325_574_000u64;
    let gate = Gate::protect(ProviderKind::Claude, base + 1).unwrap();
    let _health = test_health::Clear::new(&gate);
    // (channel, `.channel` binding, unknown targets held, killed)
    let cases = [
        (base + 1, true, false, false),
        (base + 2, true, false, true),
        (base + 3, false, true, false),
        (base + 4, false, false, true),
    ];
    let mut guards = Vec::new();
    for (channel, bound, hold, expected) in cases {
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
        let _hold = hold.then(HoldUnknownForTest::new);
        super::clean_orphan_sessions(&shared, std::slice::from_ref(&name)).await;
        assert_eq!(killed(&name), expected, "{name}");
    }
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
