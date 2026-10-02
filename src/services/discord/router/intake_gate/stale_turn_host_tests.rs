//! The stale-turn releases on every stored host case and tmux answer, from their entries.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use poise::serenity_prelude::{ChannelId, MessageId, UserId};

use super::*;
use crate::services::discord::admin_host_guard::tests::{Host, ModeTmux, Tmux};
use crate::services::discord::health::HealthRegistry;
use crate::services::discord::host_defer_gate::tests::{map_channel, postgres};
use crate::services::discord::host_teardown_gate::test_support::{channel_key, shared_on};
use crate::services::provider::CancelToken;
use crate::services::provider::cancel_token_cleanup::authority::TmuxBinding;

/// A legacy row stored under the session key alone, which no channel lookup finds: only the
/// inflight row's name reaches it.
const KEY_ONLY_LEGACY: usize = Host::ALL.len();

/// A turn on `channel` bound to `name`, if any, whose inflight row naming it went stale.
async fn stale_turn(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
    name: Option<&str>,
) -> Arc<CancelToken> {
    let token = Arc::new(CancelToken::new());
    let binding = name.map(|name| TmuxBinding::NameOnly {
        name: name.to_string(),
    });
    *token.tmux_binding.lock().unwrap() = binding;
    let message = MessageId::new(channel.get() + 1);
    let start = crate::services::discord::mailbox_try_start_turn;
    assert!(start(shared, channel, token.clone(), UserId::new(7), message).await);
    shared.restart.global_active.store(1, Ordering::Relaxed);
    let mut row = crate::services::discord::inflight::InflightTurnState::new(
        provider.clone(),
        channel.get(),
        None,
        1,
        message.get(),
        message.get() + 1,
        "stale turn fixture".to_string(),
        None,
        name.map(str::to_string),
        None,
        None,
        0,
    );
    let threshold = crate::services::discord::inflight::INFLIGHT_STALENESS_THRESHOLD_SECS as i64;
    let stale = chrono::Local::now() - chrono::Duration::seconds(threshold + 5);
    row.updated_at = stale.format("%Y-%m-%d %H:%M:%S").to_string();
    row.started_at = row.updated_at.clone();
    row.turn_nonce = token.turn_nonce().map(str::to_string);
    let root = crate::services::discord::inflight::inflight_runtime_root().expect("runtime root");
    let dir = root.join(provider.as_str());
    std::fs::create_dir_all(&dir).unwrap();
    let json = serde_json::to_string_pretty(&row).unwrap();
    std::fs::write(dir.join(format!("{}.json", channel.get())), json).unwrap();
    token
}

/// Whether the turn still owns its mailbox, uncancelled, with its row and counter.
async fn turn_kept(shared: &Arc<SharedData>, channel: ChannelId, token: &Arc<CancelToken>) -> bool {
    let after = crate::services::discord::mailbox_snapshot(shared, channel).await;
    let owned = after
        .cancel_token
        .as_ref()
        .is_some_and(|t| Arc::ptr_eq(t, token));
    let row = crate::services::discord::inflight::load_inflight_state(
        &ProviderKind::Claude,
        channel.get(),
    );
    owned
        && !token.cancelled.load(Ordering::Relaxed)
        && row.is_some()
        && shared.restart.global_active.load(Ordering::Relaxed) == 1
}

/// Each host case on its own channel under `mode`, as `(label, channel, name, admitted,
/// tmux reports the session up)`.
async fn seeded_cases(
    pool: &sqlx::PgPool,
    shared: &SharedData,
    base: u64,
    mode: Tmux,
    m: usize,
) -> Vec<(String, ChannelId, String, bool, bool)> {
    let provider = ProviderKind::Claude;
    let mut cases = Vec::new();
    for n in 0..=KEY_ONLY_LEGACY {
        let channel = ChannelId::new(base + (m * 100 + n) as u64);
        let name = provider.build_tmux_session_name(&format!("p4r1b-stale-{base}-{m}-{n}"));
        let key = channel_key(shared, &name);
        let (label, admitted, probed) = match Host::ALL.get(n) {
            Some(host) => {
                host.seed(pool, &key, &name, channel.get()).await;
                (format!("{mode:?} {host:?}"), host.admitted(), host.probed())
            }
            None => {
                let seed = crate::services::discord::host_key_derivation::tests::seed_row;
                seed(pool, "claude", None, &key, channel.get(), None).await;
                (format!("{mode:?} key-only legacy"), true, true)
            }
        };
        cases.push((label, channel, name, admitted, probed && mode == Tmux::Live));
    }
    cases
}

/// A `provider` runtime on `pool` its own health registry snapshots.
async fn registered_runtime(
    pool: &sqlx::PgPool,
    provider: &ProviderKind,
) -> (Arc<SharedData>, Arc<HealthRegistry>) {
    let mut shared = shared_on(pool).await;
    shared.settings.write().await.provider = provider.clone();
    let registry = Arc::new(HealthRegistry::new());
    Arc::get_mut(&mut shared)
        .expect("an unshared runtime")
        .health_registry = Arc::downgrade(&registry);
    let name = provider.as_str().to_string();
    registry.register(name, shared.clone()).await;
    (shared, registry)
}

// The queue guard releases a stale turn with no live owner only for a confirmed legacy
// session; for any other host it reports the turn active and changes nothing.
#[tokio::test]
async fn queue_guard_keeps_a_stale_turn_on_an_unconfirmed_host_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let tmux = ModeTmux::install();
    let (db, pool) = postgres().await;
    let provider = ProviderKind::Claude;
    let (shared, _registry) = registered_runtime(&pool, &provider).await;
    for (m, mode) in Tmux::ALL.into_iter().enumerate() {
        tmux.serve(mode);
        let cases = seeded_cases(&pool, &shared, 1_479_671_342_000_000_000, mode, m).await;
        for (label, channel, name, admitted, up) in cases {
            let token = stale_turn(&shared, &provider, channel, Some(&name)).await;
            tmux.take_writes();

            let active =
                mailbox_has_live_active_turn_or_cleanup_stale_proof(&shared, &provider, channel)
                    .await;

            // A session tmux reports up has a live owner, so main keeps it too.
            if admitted && !up {
                assert!(!active, "{label}: released as in main");
                assert!(token.cancelled.load(Ordering::Relaxed), "{label}");
                continue;
            }
            assert!(active, "{label}: a kept turn reads active");
            assert!(turn_kept(&shared, channel, &token).await, "{label}");
            assert_eq!(tmux.take_writes(), Vec::<String>::new(), "{label}");
        }
    }
    db.drop().await;
}

// The intake thread guard force-cleans a stale thread only for a confirmed legacy session;
// for any other host it answers `false`, which queues the message, and changes nothing.
#[tokio::test]
async fn thread_guard_keeps_a_stale_thread_on_an_unconfirmed_host_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let tmux = ModeTmux::install();
    let (db, pool) = postgres().await;
    let provider = ProviderKind::Claude;
    let (shared, _registry) = registered_runtime(&pool, &provider).await;
    for (m, mode) in Tmux::ALL.into_iter().enumerate() {
        tmux.serve(mode);
        let cases = seeded_cases(&pool, &shared, 1_479_671_343_000_000_000, mode, m).await;
        for (label, channel, name, admitted, _) in cases {
            let token = stale_turn(&shared, &provider, channel, Some(&name)).await;
            tmux.take_writes();
            let now = chrono::Utc::now().timestamp();

            let clean =
                thread_guard_should_force_clean_stale_thread(&shared, &provider, channel, now)
                    .await;

            // A stale turn on a live tmux session with no watcher reads desynced, so main
            // cleans it whatever tmux answers.
            assert_eq!(clean, admitted, "{label}");
            assert!(turn_kept(&shared, channel, &token).await, "{label}");
            assert_eq!(tmux.take_writes(), Vec::<String>::new(), "{label}");
        }
    }
    db.drop().await;
}

// A process-backed provider's stale turn names no tmux session and its channel name is not
// read as one: the queue guard releases it as in main.
#[tokio::test]
async fn queue_guard_still_releases_a_process_turn_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let tmux = ModeTmux::install();
    tmux.serve(Tmux::Dead);
    let (db, pool) = postgres().await;
    let provider = ProviderKind::Gemini;
    let (shared, _registry) = registered_runtime(&pool, &provider).await;
    let channel = ChannelId::new(1_479_671_344_000_000_001);
    map_channel(&shared, channel, "p4r1b-process").await;
    let token = stale_turn(&shared, &provider, channel, None).await;

    let active =
        mailbox_has_live_active_turn_or_cleanup_stale_proof(&shared, &provider, channel).await;

    assert!(!active, "released as in main");
    assert!(token.cancelled.load(Ordering::Relaxed));
    db.drop().await;
}
