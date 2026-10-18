//! The restart-time orphan finalize against the stored rows the host guard reads.

use std::sync::Arc;

use poise::serenity_prelude::{self as serenity, ChannelId};

use super::cleanup_routing_orphaned_inflight;
use crate::services::discord::host_teardown_gate::test_support::{
    Stored, busy_turn, channel_key, seed, shared_on, turn_kept,
};
use crate::services::discord::inflight;
use crate::services::discord::recovery_engine::o_cut_recorder;
use crate::services::discord::restart_report::{self, RestartReportContext};
use crate::services::discord::settings::BotChannelRoutingGuardFailure;
use crate::services::provider::ProviderKind;
use crate::services::session_host::test_support::InjectedLivenessGuard;
use crate::services::session_host::{HostLiveness, HostSessionRef};

// A dead pane is finalized only for a found legacy row or no row at all; any other stored or
// marked host keeps its restart report, turn and row, with no notice, though Discord answers.
#[tokio::test]
async fn routing_orphan_finalizes_only_a_dead_pane_the_host_guard_admits_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _legacy_output = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let shared = shared_on(&pool).await;
    let provider = ProviderKind::Claude;
    for (n, stored) in Stored::ALL.into_iter().enumerate() {
        let channel = ChannelId::new(1_479_671_301_387_120_000 + n as u64);
        let name = provider.build_tmux_session_name(&format!("p4b1-orphan-final-{n}"));
        seed(
            &pool,
            &channel_key(&shared, &name),
            &name,
            channel.get(),
            stored,
        )
        .await;
        let dead = HostLiveness::DeadOrAbsent;
        let _pane = InjectedLivenessGuard::set(HostSessionRef::tmux(&name), dead);
        let token = busy_turn(&shared, channel, &name).await;
        let context =
            RestartReportContext::from_bridge(provider.clone(), channel.get(), None, None);
        restart_report::announce_restart(&context).expect("restart report");
        let state = inflight::load_inflight_state(&provider, channel.get()).expect("row");
        let discord = o_cut_recorder::start(channel.get()).await;

        let reason = BotChannelRoutingGuardFailure::ProviderMismatch;
        let http = &discord.http;
        cleanup_routing_orphaned_inflight(http, &shared, &provider, &state, Some(&name), reason)
            .await;

        let admitted = matches!(stored, Stored::Legacy | Stored::Missing);
        let report = restart_report::load_restart_report(&provider, channel.get());
        assert_eq!(report.is_some(), !admitted, "{stored:?}: restart report");
        assert_eq!(discord.calls().is_empty(), !admitted, "{stored:?}: notice");
        let kept = turn_kept(&shared, channel, &token).await;
        assert_eq!(kept, !admitted, "{stored:?}: turn and inflight row");
    }
    pool.close().await;
    db.drop().await;
}

// Past the relay budget a failed notice clears the row only for a confirmed dead pane; a probe
// error, or a row naming no session at all, is unmeasured and keeps the row for the next boot.
#[tokio::test]
async fn routing_orphan_budget_clear_needs_a_confirmed_dead_pane_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _legacy_output = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let shared = shared_on(&pool).await;
    let provider = ProviderKind::Claude;
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = closed.local_addr().unwrap();
    drop(closed);
    let http: Arc<serenity::Http> = Arc::new(
        serenity::HttpBuilder::new("test-token")
            .proxy(format!("http://{address}"))
            .ratelimiter_disabled(true)
            .build(),
    );
    let cases = [
        (None, true),
        (Some(HostLiveness::ProbeError), true),
        (Some(HostLiveness::DeadOrAbsent), false),
    ];
    for (n, (pane, kept)) in cases.into_iter().enumerate() {
        let channel = ChannelId::new(1_479_671_301_387_121_000 + n as u64);
        let name = provider.build_tmux_session_name(&format!("i6041-orphan-budget-{n}"));
        let _pane = pane.map(|pane| InjectedLivenessGuard::set(HostSessionRef::tmux(&name), pane));
        busy_turn(&shared, channel, &name).await;
        let mut state = inflight::load_inflight_state(&provider, channel.get()).expect("row");
        state.recovery_relay_attempts = inflight::RECOVERY_RELAY_RESTART_ATTEMPT_BUDGET - 1;
        if pane.is_none() {
            state.tmux_session_name = None;
        }
        inflight::save_inflight_state(&state).expect("row at the budget edge");
        let session = state.tmux_session_name.as_deref();

        let reason = BotChannelRoutingGuardFailure::ProviderMismatch;
        cleanup_routing_orphaned_inflight(&http, &shared, &provider, &state, session, reason).await;

        let row = inflight::load_inflight_state(&provider, channel.get());
        assert_eq!(row.is_some(), kept, "{pane:?}: inflight row");
    }
    pool.close().await;
    db.drop().await;
}
