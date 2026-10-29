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

// Consumer contract: use the production running-only writer, then finish the run.
async fn receipt_run(pool: &sqlx::PgPool, run: &str, routine: &str, receipt: &str) {
    sqlx::query("INSERT INTO routine_runs (id, routine_id, status) VALUES ($1, $2, 'running')")
        .bind(run)
        .bind(routine)
        .execute(pool)
        .await
        .unwrap();
    let store =
        crate::services::routines::store::RoutineStore::new_with_timezone_and_checkpoint_limit(
            Arc::new(pool.clone()),
            "UTC",
            4096,
        );
    assert!(
        store
            .set_run_owned_tmux_session(run, receipt)
            .await
            .unwrap()
    );
    sqlx::query("UPDATE routine_runs SET status = 'succeeded' WHERE id = $1")
        .bind(run)
        .execute(pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn receipt_backstop_kills_only_the_admitted_exact_session_pg() {
    if !n3bc_isolated("receipt_backstop_kills_only_the_admitted_exact_session_pg") {
        return;
    }
    let (db, pool) = postgres().await;
    let (shared, _registry) = runtime(&pool).await;
    let routine = "receipt-backstop-target";
    let (target, fallback) = fresh_routine(&pool, routine).await;
    let unrelated = ProviderKind::Claude.build_tmux_session_name("receipt-unrelated");
    let target_key = channel_key(&shared, &target);
    let fallback_key = channel_key(&shared, &fallback);
    seed(&pool, &target_key, &target, 7_157_001, Stored::Legacy).await;
    seed(&pool, &fallback_key, &fallback, 7_157_002, Stored::Hosted).await;
    receipt_run(&pool, "receipt-target-run", routine, &target_key).await;
    receipt_run(&pool, "receipt-fallback-run", routine, &fallback_key).await;
    let names = vec![target.clone(), fallback.clone(), unrelated.clone()];
    let panes: Vec<_> = names
        .iter()
        .map(|name| {
            own(name);
            PaneLivenessOverrideGuard::set(name, PaneLiveness::DeadOrAbsent)
        })
        .collect();
    super::reap_listed_dead_sessions(&shared, &names).await;
    assert_eq!(
        crate::services::platform::tmux::kill_requests::count(&target),
        1
    );
    assert!(
        !killed(&fallback),
        "the newer protected fallback receipt stays intact"
    );
    assert!(
        !killed(&unrelated),
        "the receipt grants no authority over another name"
    );
    let requested = std::fs::read_to_string(n3bc_root().join("tmux-kills")).unwrap();
    assert_eq!(requested.lines().count(), 1, "{requested}");
    assert!(requested.contains(&format!("-t ={target}")), "{requested}");
    drop(panes);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn receipt_backstop_keeps_same_name_when_foreign_token_receipt_is_protected_pg() {
    if !n3bc_isolated("receipt_backstop_keeps_same_name_when_foreign_token_receipt_is_protected_pg")
    {
        return;
    }
    let (db, pool) = postgres().await;
    let (shared, _registry) = runtime(&pool).await;
    let routine = "receipt-backstop-foreign";
    let (target, _) = fresh_routine(&pool, routine).await;
    let local_key = channel_key(&shared, &target);
    let foreign_key = format!("claude/foreign-token/mac-mini:{target}");
    seed(&pool, &local_key, &target, 7_157_003, Stored::Legacy).await;
    seed(&pool, &foreign_key, &target, 7_157_004, Stored::Hosted).await;
    receipt_run(&pool, "receipt-foreign-run", routine, &foreign_key).await;
    receipt_run(&pool, "receipt-local-run", routine, &local_key).await;
    let before: serde_json::Value =
        sqlx::query_scalar("SELECT to_jsonb(s) FROM sessions AS s WHERE session_key = $1")
            .bind(&foreign_key)
            .fetch_one(&pool)
            .await
            .unwrap();
    own(&target);
    let _pane = PaneLivenessOverrideGuard::set(&target, PaneLiveness::DeadOrAbsent);
    super::reap_listed_dead_sessions(&shared, std::slice::from_ref(&target)).await;
    assert!(
        !killed(&target),
        "the exact local receipt cannot bypass a protected foreign receipt for the same bare name"
    );
    let after: serde_json::Value =
        sqlx::query_scalar("SELECT to_jsonb(s) FROM sessions AS s WHERE session_key = $1")
            .bind(&foreign_key)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(after, before);
    assert!(!n3bc_root().join("tmux-kills").exists());
    pool.close().await;
    db.drop().await;
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

// A bound Legacy orphan with a row is killed by the public pass and its old files go.
#[tokio::test]
async fn n3bc_public_bound_legacy_orphan_cleanup_removes_old_files_pg() {
    if !n3bc_isolated("n3bc_public_bound_legacy_orphan_cleanup_removes_old_files_pg") {
        return;
    }
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
    super::cleanup_orphan_tmux_sessions(&shared).await;
    assert!(killed(&name), "the public cleanup kills the bound orphan");
    assert!(
        !std::path::Path::new(&file).exists(),
        "the cleanup removes old files"
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn n3bc_public_cleanup_mixed_owned_and_rowless_legacy_orphan_pg() {
    use crate::db::dispatched_sessions::hosted_execution::{
        HostedLookup, HostedLookupKey, load_hosted_execution_pg,
    };

    if !n3bc_isolated("n3bc_public_cleanup_mixed_owned_and_rowless_legacy_orphan_pg") {
        return;
    }
    let (db, pool) = postgres().await;
    let (shared, _registry) = runtime(&pool).await;
    let channel = ChannelId::new(6_325_575_950);
    let channel_name = "n3bc-mixed-owned";
    let owned = ProviderKind::Claude.build_tmux_session_name(channel_name);
    let orphan = ProviderKind::Claude.build_tmux_session_name("n3bc-mixed-rowless-orphan");
    map_channel(&shared, channel, channel_name).await;
    for name in [&owned, &orphan] {
        own(name);
    }
    crate::services::tmux_common::write_tmux_channel_binding(&orphan, channel.get() + 1).unwrap();
    let orphan_key = channel_key(&shared, &orphan);
    assert!(
        matches!(
            load_hosted_execution_pg(&pool, HostedLookupKey::SessionKey(&orphan_key)).await,
            HostedLookup::Missing
        ),
        "the orphan has no sessions row"
    );
    std::fs::write(
        n3bc_root().join("tmux-sessions"),
        format!("{owned}\n{orphan}\n"),
    )
    .unwrap();
    assert!(!killed(&owned) && !killed(&orphan));

    super::cleanup_orphan_tmux_sessions(&shared).await;

    assert!(
        killed(&orphan),
        "the public mixed-list pass cleans the orphan"
    );
    assert!(!killed(&owned), "the mapped owner's session survives");
    assert!(shared.core.lock().await.sessions.contains_key(&channel));
    let kills = std::fs::read_to_string(n3bc_root().join("tmux-kills")).unwrap();
    assert!(kills.contains(&orphan));
    assert!(!kills.contains(&owned));
    pool.close().await;
    db.drop().await;
}
