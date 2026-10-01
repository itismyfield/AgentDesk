//! Restart recovery of a Claude inflight row against the host evidence it reads first.

use poise::serenity_prelude::ChannelId;

use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::discord::host_teardown_gate::test_support::{
    Stored, busy_turn, channel_key, seed, shared_on,
};
use crate::services::discord::recovery_engine::o_cut_recorder;
use crate::services::discord::restart_report::{self, RestartReportContext};
use crate::services::session_host::test_support::{InjectedLivenessGuard, InjectedPresenceGuard};
use crate::services::session_host::{HostLiveness, HostPresence, HostSessionRef};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    /// Row and report gone after the notice.
    Disposed,
    /// Report cleared, session registered, row kept for the watcher.
    Reattached,
    /// Row and report as stored, nothing registered.
    Kept,
}

/// Row shapes: `Busy` stops before the pane re-check; `Tui` carries a transcript and reaches it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Row {
    Busy,
    Tui,
}

struct Case {
    channel: ChannelId,
    label: String,
    report: bool,
    outcome: Outcome,
}

type Plan = (Stored, HostLiveness, HostPresence, bool, Row, Outcome);

fn plan() -> Vec<Plan> {
    use HostLiveness::{DeadOrAbsent, Live, ProbeError};
    use HostPresence::{Missing, Present, ProbeFailed};
    let mut plan = Vec::new();
    for report in [true, false] {
        for stored in Stored::ALL {
            let admitted = matches!(stored, Stored::Legacy | Stored::Missing);
            let outcome = if admitted {
                Outcome::Disposed
            } else {
                Outcome::Kept
            };
            plan.push((stored, DeadOrAbsent, Missing, report, Row::Busy, outcome));
        }
        let failed = (Stored::Legacy, ProbeError, ProbeFailed, report);
        plan.push((
            failed.0,
            failed.1,
            failed.2,
            failed.3,
            Row::Busy,
            Outcome::Kept,
        ));
    }
    let reattached = Outcome::Reattached;
    plan.push((Stored::Legacy, Live, Present, true, Row::Busy, reattached));
    let herdr = Stored::LegacyHerdrMarker;
    plan.push((herdr, Live, Present, true, Row::Busy, Outcome::Kept));
    plan.push((Stored::Legacy, Live, Present, false, Row::Tui, reattached));
    plan.push((
        Stored::Legacy,
        ProbeError,
        Present,
        false,
        Row::Tui,
        Outcome::Kept,
    ));
    plan.push((
        Stored::Hosted,
        DeadOrAbsent,
        Present,
        false,
        Row::Tui,
        Outcome::Kept,
    ));
    plan
}

// A Claude row moves only on a local tmux answer, and a death only when the host guard
// admits it; another host, a kept row or a failed probe keeps row and report untouched.
#[tokio::test]
async fn restart_recovery_moves_a_claude_row_only_on_a_local_tmux_answer_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _legacy_output = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let shared = shared_on(&pool).await;
    let provider = ProviderKind::Claude;
    let (mut cases, mut panes, mut presences) = (Vec::new(), Vec::new(), Vec::new());
    let transcripts = tempfile::tempdir().unwrap();
    for (n, (stored, pane, presence, report, shape, outcome)) in plan().into_iter().enumerate() {
        let channel = ChannelId::new(1_479_671_301_387_150_000 + n as u64);
        let name = provider.build_tmux_session_name(&format!("p5c-restart-{n}"));
        let key = channel_key(&shared, &name);
        seed(&pool, &key, &name, channel.get(), stored).await;
        let session = HostSessionRef::tmux(&name);
        panes.push(InjectedLivenessGuard::set(session, pane));
        presences.push(InjectedPresenceGuard::set(session, presence));
        busy_turn(&shared, channel, &name).await;
        if shape == Row::Tui {
            let transcript = transcripts.path().join(format!("{n}.jsonl"));
            std::fs::write(&transcript, "").unwrap();
            let mut row = inflight::load_inflight_state(&provider, channel.get()).unwrap();
            row.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
            row.output_path = Some(transcript.display().to_string());
            inflight::save_inflight_state(&row).unwrap();
        }
        if report {
            let context =
                RestartReportContext::from_bridge(provider.clone(), channel.get(), None, None);
            restart_report::announce_restart(&context).expect("restart report");
        }
        let label = format!("{stored:?} {pane:?} {presence:?} report={report} {shape:?}");
        cases.push(Case {
            channel,
            label,
            report,
            outcome,
        });
    }
    let discord = o_cut_recorder::start(cases[0].channel.get()).await;

    restore_inflight_turns(&discord.http, &shared, &provider).await;

    for case in &cases {
        let (label, channel) = (&case.label, case.channel.get());
        let row = inflight::load_inflight_state(&provider, channel).is_some();
        let report = restart_report::load_restart_report(&provider, channel).is_some();
        let registered = shared
            .core
            .lock()
            .await
            .sessions
            .contains_key(&case.channel);
        let observed = match (row, report, registered) {
            (false, false, false) => Outcome::Disposed,
            (true, false, true) => Outcome::Reattached,
            (true, kept_report, false) if kept_report == case.report => Outcome::Kept,
            other => panic!("{label}: row/report/registered = {other:?}"),
        };
        assert_eq!(observed, case.outcome, "{label}");
    }
    let disposed = cases
        .iter()
        .filter(|c| c.outcome == Outcome::Disposed)
        .count();
    assert!(
        discord.contents().len() >= disposed,
        "each disposed row notifies"
    );
    drop((panes, presences));
    pool.close().await;
    db.drop().await;
}
