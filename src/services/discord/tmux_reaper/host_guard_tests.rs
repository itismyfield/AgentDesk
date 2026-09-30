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

/// Whether the session got main's teardown, read from the exit reason written before the kill.
fn killed(name: &str) -> bool {
    let exit_reason = crate::services::tmux_common::session_temp_path(name, "exit_reason");
    std::path::Path::new(&exit_reason).exists()
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

// The fresh-routine backstop keys the row its latest owning run recorded, which a
// channel-style key would miss; with no recorded key it falls back to the orphan rule.
#[tokio::test]
async fn fresh_routine_backstop_takes_the_owned_row_as_ownership_proof_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (db, pool) = postgres().await;
    let (shared, _registry) = runtime(&pool).await;
    let claude = ProviderKind::Claude;
    // (owned run recorded, what the stored rows say, killed)
    let cases = [
        (true, Stored::Hosted, false),
        (true, Stored::Legacy, true),
        (false, Stored::Missing, true),
        (false, Stored::MissingHerdrMarker, false),
    ];
    for (n, (owned, stored, expected)) in cases.into_iter().enumerate() {
        let routine_id = format!("p4a-routine-{n}");
        let name = claude.build_tmux_session_name(&format!("routine p4a-{n} - agent"));
        sqlx::query(
            "INSERT INTO routines (id, agent_id, script_ref, name, execution_strategy)
             VALUES ($1, 'agent', 'script', $1, 'fresh')",
        )
        .bind(&routine_id)
        .execute(&pool)
        .await
        .unwrap();
        // A routine session's row key is not the channel-style key for its tmux name.
        let key = format!("claude/routine-token/mac-mini:{name}");
        let channel = 1_479_671_301_387_060_300 + n as u64;
        seed(&pool, &key, &name, channel, stored).await;
        if owned {
            sqlx::query(
                "INSERT INTO routine_runs (id, routine_id, status, owned_tmux_session)
                 VALUES ($1, $2, 'succeeded', $3)",
            )
            .bind(format!("{routine_id}-run"))
            .bind(&routine_id)
            .bind(&key)
            .execute(&pool)
            .await
            .unwrap();
        }
        let _pane = PaneLivenessOverrideGuard::set(&name, PaneLiveness::DeadOrAbsent);
        super::reap_fresh_routine_orphan(&shared, &claude, &name, &routine_id).await;
        assert_eq!(killed(&name), expected, "{owned} {stored:?}");
    }
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
