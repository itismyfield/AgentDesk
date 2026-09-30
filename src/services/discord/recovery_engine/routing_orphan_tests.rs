//! The restart-time orphan finalize against the stored rows the host guard reads.

use poise::serenity_prelude::ChannelId;

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
