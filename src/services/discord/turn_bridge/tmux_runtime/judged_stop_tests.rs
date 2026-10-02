//! Judged channel stops at their production entries: the judge writes nothing, a refused host
//! keeps its turn, and the cancel, bind and tombstone act on what the judge read.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use poise::serenity_prelude::{ChannelId, MessageId, UserId};

use super::super::stop_host::tests::{
    FakeHerdr, Fixture, Mark, NOT_TMUX, SERVERS, Server, bound_token, generating_turn,
    herdr_target, mark, run,
};
use super::*;
use crate::services::discord::health::{HealthRegistry, InflightDisposition};
use crate::services::session_host::HostMutation;
use crate::services::turn_lifecycle::TurnLifecycleTarget;

async fn runtime() -> (Arc<SharedData>, Arc<HealthRegistry>) {
    let shared = crate::services::discord::make_shared_data_for_tests();
    let registry = Arc::new(HealthRegistry::new());
    registry
        .register("claude".to_string(), shared.clone())
        .await;
    (shared, registry)
}

async fn start(shared: &SharedData, channel: ChannelId, token: &Arc<CancelToken>) {
    let user_msg = MessageId::new(channel.get() + 1);
    let start = crate::services::discord::mailbox_try_start_turn;
    assert!(start(shared, channel, token.clone(), UserId::new(7), user_msg).await);
}

/// Saves `provider`'s inflight row on `channel` naming `name`; with `compat`, as an older build
/// stored it with no finalizer id, so a writable load backfills and saves it again.
fn inflight_row(provider: &ProviderKind, channel: ChannelId, name: &str, compat: bool) -> PathBuf {
    let user_msg = channel.get() + 1;
    let row = inflight::InflightTurnState::new(
        provider.clone(),
        channel.get(),
        None,
        1,
        user_msg,
        user_msg + 1,
        "judged stop fixture".to_string(),
        None,
        Some(name.to_string()),
        None,
        Some("fifo".to_string()),
        0,
    );
    inflight::save_inflight_state_create_new(&row).expect("persist the inflight row");
    let root = inflight::inflight_runtime_root().expect("inflight root");
    let path = inflight::inflight_state_path(&root, provider, channel.get());
    if compat {
        let raw = std::fs::read_to_string(&path).unwrap();
        let mut raw: serde_json::Value = serde_json::from_str(&raw).unwrap();
        raw.as_object_mut().unwrap().remove("finalizer_turn_id");
        std::fs::write(&path, raw.to_string()).unwrap();
    }
    path
}

/// The row's bytes and modification time, which any save changes.
fn file_state(path: &PathBuf) -> Option<(Vec<u8>, std::time::SystemTime)> {
    let bytes = std::fs::read(path).ok()?;
    Some((bytes, std::fs::metadata(path).ok()?.modified().ok()?))
}

fn tombstone(channel: ChannelId) -> Option<Option<String>> {
    let stop = crate::services::discord::tmux::recent_turn_stop_for_channel(channel);
    stop.map(|stop| stop.tmux_session_name)
}

async fn mailbox_holds(shared: &SharedData, channel: ChannelId, token: &Arc<CancelToken>) -> bool {
    let snapshot = crate::services::discord::mailbox_snapshot(shared, channel).await;
    snapshot
        .cancel_token
        .is_some_and(|held| Arc::ptr_eq(&held, token))
}

// A preserve stop on another host's turn is kept before its first write on every tmux
// condition: no tombstone, cancel, inflight backfill or tmux call; a legacy turn is stopped.
#[test]
fn a_preserve_stop_keeps_a_turn_on_another_host_before_any_write() {
    let fx = Fixture::new();
    run(async {
        let (shared, registry) = runtime().await;
        let mut n = 0;
        for server in SERVERS {
            fx.serve(server);
            for host in NOT_TMUX.into_iter().chain([Mark::Absent]) {
                n += 1;
                let channel = ChannelId::new(5_340_610_000 + n * 10);
                let name = format!("AgentDesk-claude-p6asb-tl-{n}");
                mark(&name, host);
                let token = bound_token(&ProviderKind::Claude, &name);
                start(&shared, channel, &token).await;
                let row = inflight_row(&ProviderKind::Claude, channel, &name, true);
                let before = file_state(&row);
                let _ = fx.take_calls();
                let target = TurnLifecycleTarget {
                    provider: Some(ProviderKind::Claude),
                    channel_id: Some(channel),
                    tmux_name: name.clone(),
                };
                let stop = crate::services::turn_lifecycle::stop_turn_preserving_queue;
                let result = stop(Some(&registry), &target, "p6asb").await;

                let case = format!("{server:?} {host:?}");
                let legacy = matches!(host, Mark::Absent);
                assert_eq!(result.host_guard_kept(), !legacy, "{case}");
                assert_eq!(token.cancelled.load(Ordering::SeqCst), legacy, "{case}");
                if legacy {
                    continue;
                }
                assert!(mailbox_holds(&shared, channel, &token).await, "{case}");
                assert_eq!(tombstone(channel), None, "{case}");
                assert_eq!(file_state(&row), before, "{case}: no backfill");
                assert_eq!(fx.take_calls(), Vec::<String>::new(), "{case}");
            }
        }
    });
}

// A user stop is judged before any write: another host's turn is left as it is, its inflight
// row unsaved; an admitted unbound turn is bound to, and tombstoned under, its row's name
// without saving that row, for Claude and for Codex, whose stop offset reads the Codex row.
#[test]
fn a_command_stop_judges_before_any_write_and_binds_what_it_judged() {
    let fx = Fixture::new();
    run(async {
        let shared = crate::services::discord::make_shared_data_for_tests();
        let mut n = 0;
        for server in SERVERS {
            fx.serve(server);
            for host in NOT_TMUX {
                n += 1;
                let channel = ChannelId::new(5_340_620_000 + n * 10);
                let name = format!("AgentDesk-claude-p6asb-cmd-refused-{n}");
                mark(&name, host);
                let token = Arc::new(CancelToken::new());
                start(&shared, channel, &token).await;
                let row = inflight_row(&ProviderKind::Claude, channel, &name, true);
                let before = file_state(&row);
                let _ = fx.take_calls();

                let stop = begin_command_stop(&shared, &ProviderKind::Claude, channel, true).await;

                let case = format!("{server:?} {host:?}");
                assert!(matches!(stop, CommandStop::HostRefused), "{case}");
                assert!(!token.cancelled.load(Ordering::SeqCst), "{case}");
                assert!(mailbox_holds(&shared, channel, &token).await, "{case}");
                assert_eq!(token.tmux_session_name(), None, "{case}: not bound");
                assert_eq!(tombstone(channel), None, "{case}");
                assert_eq!(file_state(&row), before, "{case}: no backfill");
                assert_eq!(fx.take_calls(), Vec::<String>::new(), "{case}");
            }
            for provider in [ProviderKind::Claude, ProviderKind::Codex] {
                n += 1;
                let channel = ChannelId::new(5_340_620_000 + n * 10);
                let name = format!("AgentDesk-{}-p6asb-cmd-admitted-{n}", provider.as_str());
                mark(&name, Mark::Absent);
                let token = Arc::new(CancelToken::new());
                start(&shared, channel, &token).await;
                let row = inflight_row(&provider, channel, &name, true);
                let before = file_state(&row);

                let stop = begin_command_stop(&shared, &provider, channel, true).await;

                let case = format!("{server:?} {provider:?}");
                assert!(matches!(stop, CommandStop::Stop(_)), "{case}");
                assert!(token.cancelled.load(Ordering::SeqCst), "{case}");
                assert_eq!(token.tmux_session_name().as_deref(), Some(name.as_str()));
                assert_eq!(tombstone(channel), Some(Some(name.clone())), "{case}");
                assert_eq!(file_state(&row), before, "{case}: no backfill");
            }
        }
    });
}

// The cancel binds the name the judge admitted and registers its pane's provider PID, even when
// the marker turns Herdr between the judge and the cancel: the verdict is not taken again.
#[test]
fn a_command_stop_binds_its_verdict_when_the_marker_changes_after_it() {
    let fx = Fixture::new();
    run(async {
        let shared = crate::services::discord::make_shared_data_for_tests();
        fx.serve(Server::Live);
        let channel = ChannelId::new(5_340_630_000);
        let name = "AgentDesk-codex-p6asb-cmd-marker-after";
        mark(name, Mark::Absent);
        let token = Arc::new(CancelToken::new());
        start(&shared, channel, &token).await;
        inflight_row(&ProviderKind::Codex, channel, name, false);
        let judge = ChannelStop::judge(&shared, &ProviderKind::Codex, channel, None, true);
        let stop = judge.await.expect("an active turn");
        assert!(!stop.refused());
        mark(name, Mark::Herdr);

        assert!(stop.cancel().await.token.is_some());

        assert_eq!(token.tmux_session_name().as_deref(), Some(name));
        assert_eq!(
            token.child_pid_value(),
            Some(fx.pid()),
            "the judged pane's PID"
        );
    });
}

// A cancel acts only on the turn it judged: a turn that replaced it meanwhile is neither
// cancelled nor tombstoned.
#[test]
fn a_judged_cancel_leaves_a_turn_that_replaced_the_judged_one() {
    let _fx = Fixture::new();
    run(async {
        let shared = crate::services::discord::make_shared_data_for_tests();
        let channel = ChannelId::new(5_340_640_000);
        let name = "AgentDesk-claude-p6asb-cas";
        mark(name, Mark::Absent);
        let judged = bound_token(&ProviderKind::Claude, name);
        start(&shared, channel, &judged).await;
        let judge = ChannelStop::judge(&shared, &ProviderKind::Claude, channel, None, false);
        let stop = judge.await.expect("an active turn");
        let finish = crate::services::discord::mailbox_finish_turn;
        finish(&shared, &ProviderKind::Claude, channel).await;
        let next = bound_token(&ProviderKind::Claude, name);
        start(&shared, channel, &next).await;

        assert!(stop.cancel().await.token.is_none());

        assert!(!next.cancelled.load(Ordering::SeqCst));
        assert!(mailbox_holds(&shared, channel, &next).await);
        assert_eq!(tombstone(channel), None);
    });
}

// The idle-skip reads the session the stop judged: a token rebound afterwards to a pane that
// reads ready does not make the stop skip the interrupt and clear a row it was told to keep.
#[test]
fn an_idle_skip_reads_the_judged_session_not_a_later_binding() {
    let fx = Fixture::new();
    run(async {
        let (shared, registry) = runtime().await;
        fx.serve(Server::Live);
        let channel = ChannelId::new(5_340_650_000);
        let (judged, later) = (
            "AgentDesk-claude-p6asb-idle-a",
            "AgentDesk-claude-p6asb-idle-b",
        );
        mark(judged, Mark::Absent);
        mark(later, Mark::Absent);
        fx.ready(later);
        let token = bound_token(&ProviderKind::Claude, judged);
        start(&shared, channel, &token).await;
        let row = inflight_row(&ProviderKind::Claude, channel, judged, false);
        let judge = crate::services::discord::health::judge_provider_channel_stop;
        let stop = judge(&registry, "claude", channel).await;
        token.bind_claude_tmux_session(later);
        let restart_mode = crate::services::discord::InflightRestartMode::DrainRestart;
        let policy = TmuxCleanupPolicy::PreserveSessionAndInflight { restart_mode };

        let stop_judged = crate::services::discord::health::stop_judged_provider_channel;
        let result = stop_judged(stop, "p6asb", policy).await.unwrap();

        assert_eq!(result.inflight, InflightDisposition::NotNeeded);
        assert!(row.exists(), "the busy judged pane keeps the row");
        assert!(token.cancelled.load(Ordering::SeqCst));
    });
}

/// A Herdr target for the generating Claude turn on `session`, admitted by its gate.
fn herdr_turn(fx: &Fixture, session: &str) -> (Arc<CancelToken>, StopTarget) {
    let (token, _) = generating_turn(fx, session);
    let host = FakeHerdr::new(Ok(HostMutation::Confirmed));
    (
        token,
        herdr_target(&ProviderKind::Claude, session, host, Ok(())),
    )
}

/// Finishes the channel's turn once it is cancelled, as its source owner's exit would.
fn owner_exits_after_cancel(
    shared: &Arc<SharedData>,
    channel: ChannelId,
    token: &Arc<CancelToken>,
) {
    let (shared, token) = (shared.clone(), token.clone());
    tokio::spawn(async move {
        while !token.cancelled.load(Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        crate::services::discord::mailbox_finish_turn(&shared, &ProviderKind::Claude, channel)
            .await;
    });
}

// A Herdr stop leaves the turn's row to its source owner: a turn that outlives the wait keeps
// its mailbox, counter and row with no fallback, and one whose mailbox frees in time keeps its
// row, at the recovery stop and at the preserve-stop entry, which reports the turn kept.
#[test]
fn a_host_owned_stop_keeps_the_turn_row_for_its_owner() {
    let fx = Fixture::new();
    run(async {
        let (shared, registry) = runtime().await;
        let judge = crate::services::discord::health::judge_provider_channel_stop;
        let stop_judged = crate::services::discord::health::stop_judged_provider_channel;
        let policy = TmuxCleanupPolicy::PreserveSession;

        let (channel, session) = (ChannelId::new(5_340_660_000), "AgentDesk-claude-p6asb-ho-a");
        let (token, target) = herdr_turn(&fx, session);
        start(&shared, channel, &token).await;
        let row = inflight_row(&ProviderKind::Claude, channel, session, false);
        let active = shared.restart.global_active.load(Ordering::SeqCst);
        judge_next_as(target);
        let stop = judge(&registry, "claude", channel).await;
        let result = stop_judged(stop, "p6asb", policy).await.unwrap();
        assert_eq!(result.inflight, InflightDisposition::PreservedByHostGuard);
        assert!(
            mailbox_holds(&shared, channel, &token).await,
            "no fallback finish"
        );
        assert_eq!(shared.restart.global_active.load(Ordering::SeqCst), active);
        assert!(row.exists());

        let (channel, session) = (ChannelId::new(5_340_660_010), "AgentDesk-claude-p6asb-ho-d");
        let (token, target) = herdr_turn(&fx, session);
        start(&shared, channel, &token).await;
        let row = inflight_row(&ProviderKind::Claude, channel, session, false);
        owner_exits_after_cancel(&shared, channel, &token);
        judge_next_as(target);
        let stop = judge(&registry, "claude", channel).await;
        let result = stop_judged(stop, "p6asb", policy).await.unwrap();
        assert_eq!(result.lifecycle_path, "canonical");
        assert_eq!(result.inflight, InflightDisposition::PreservedByHostGuard);
        assert!(row.exists(), "the canonical exit clears no host-owned row");

        let (channel, session) = (
            ChannelId::new(5_340_660_020),
            "AgentDesk-claude-p6asb-ho-tl",
        );
        let (token, target) = herdr_turn(&fx, session);
        start(&shared, channel, &token).await;
        let row = inflight_row(&ProviderKind::Claude, channel, session, false);
        owner_exits_after_cancel(&shared, channel, &token);
        judge_next_as(target);
        let target = TurnLifecycleTarget {
            provider: Some(ProviderKind::Claude),
            channel_id: Some(channel),
            tmux_name: session.to_string(),
        };
        let stop = crate::services::turn_lifecycle::stop_turn_preserving_queue;
        let result = stop(Some(&registry), &target, "p6asb").await;
        assert!(result.host_guard_kept(), "the caller changes nothing more");
        assert!(row.exists());
        for session in ["ho-a", "ho-d", "ho-tl"] {
            let session = format!("AgentDesk-claude-p6asb-{session}");
            crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&session);
        }
    });
}

// A preserve stop that knows only the tmux name keeps a turn on another host it finds by that
// name; the turn found for a legacy name is finished as in main.
#[test]
fn a_name_only_preserve_stop_keeps_a_turn_on_another_host() {
    let fx = Fixture::new();
    run(async {
        let (shared, registry) = runtime().await;
        fx.serve(Server::NoSocket);
        for (n, host) in [Mark::Herdr, Mark::Absent].into_iter().enumerate() {
            let channel = ChannelId::new(5_340_670_000 + n as u64 * 10);
            let channel_name = format!("p6asb-by-name-{n}");
            let map = crate::services::discord::host_defer_gate::tests::map_channel;
            map(&shared, channel, &channel_name).await;
            let name = ProviderKind::Claude.build_tmux_session_name(&channel_name);
            mark(&name, host);
            let token = bound_token(&ProviderKind::Claude, &name);
            start(&shared, channel, &token).await;
            let target = TurnLifecycleTarget {
                provider: None,
                channel_id: None,
                tmux_name: name.clone(),
            };
            let stop = crate::services::turn_lifecycle::stop_turn_preserving_queue;
            let result = stop(Some(&registry), &target, "p6asb").await;
            let legacy = matches!(host, Mark::Absent);
            assert_eq!(result.host_guard_kept(), !legacy, "{host:?}");
            assert_eq!(mailbox_holds(&shared, channel, &token).await, !legacy);
        }
    });
}
