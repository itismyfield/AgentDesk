//! Restart recovery of a Claude inflight row against the host evidence it reads first.

use poise::serenity_prelude::ChannelId;

use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::discord::host_teardown_gate::test_support::{
    Stored, busy_turn, channel_key, seed, shared_on,
};
use crate::services::discord::recovery_engine::o_cut_recorder;
use crate::services::discord::restart_report::{self, RestartReportContext};
use crate::services::session_host::test_support::{
    InjectedLivenessGuard, InjectedPresenceGuard, inject_liveness,
};
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

/// A PATH-first tmux whose sessions exist and whose panes read dead once `dead` exists.
fn pane_flag_tmux() -> (tempfile::TempDir, crate::config::TestEnvVarGuard) {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let binary = dir.path().join("tmux");
    let body = "#!/bin/sh\n[ \"$1\" = -u ] && shift\nd=\"$(dirname \"$0\")\"\n\
                echo \"$*\" >> \"$d/calls\"\ncase \"$1\" in has-session) exit 0 ;;\n\
                list-panes) if [ -f \"$d/dead\" ]; then echo 1; else echo 0; fi; exit 0 ;;\n\
                esac\nexit 1\n";
    std::fs::write(&binary, body).unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut paths = vec![dir.path().to_path_buf()];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let path = std::env::join_paths(paths).unwrap();
    let set = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock;
    (dir, set("PATH", std::path::Path::new(&path)))
}

// After the restore reader reads its session dead, an unobserved pane hands nothing to a
// watcher and retries nothing: the row stays as stored while a live pane is handed off.
#[tokio::test]
async fn a_restored_reader_hands_off_or_retries_only_on_a_confirmed_pane_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _legacy_output = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let (tmux, _path) = pane_flag_tmux();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let shared = shared_on(&pool).await;
    let provider = ProviderKind::Claude;
    let transcripts = tempfile::tempdir().unwrap();
    let mut cases = Vec::new();
    for (n, after) in [HostLiveness::ProbeError, HostLiveness::Live]
        .into_iter()
        .enumerate()
    {
        let channel = ChannelId::new(1_479_671_301_387_160_000 + n as u64);
        let name = provider.build_tmux_session_name(&format!("p5c-reader-{n}"));
        let key = channel_key(&shared, &name);
        seed(&pool, &key, &name, channel.get(), Stored::Legacy).await;
        let session = HostSessionRef::tmux(&name);
        let pane = InjectedLivenessGuard::set(session, HostLiveness::DeadOrAbsent);
        let presence = InjectedPresenceGuard::set(session, HostPresence::Present);
        let transcript = transcripts.path().join(format!("{n}.jsonl"));
        std::fs::write(&transcript, "").unwrap();
        // A restart leaves the row with no live mailbox turn, so the recovery kicks off.
        let user_msg = channel.get() + 1;
        let text = "restored reader fixture".to_string();
        let tmux_name = Some(name.clone());
        let mut row = inflight::InflightTurnState::new(
            provider.clone(),
            channel.get(),
            None,
            1,
            user_msg,
            user_msg + 1,
            text,
            None,
            tmux_name,
            None,
            None,
            0,
        );
        row.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
        row.output_path = Some(transcript.display().to_string());
        inflight::save_inflight_state(&row).unwrap();
        cases.push((channel, name, after, pane, presence));
    }
    let discord = o_cut_recorder::start(cases[0].0.get()).await;

    restore_inflight_turns(&discord.http, &shared, &provider).await;

    // The live bridge's own bookkeeping moves these; a handoff moves the rest.
    let stored = |channel: ChannelId| {
        let row = inflight::load_inflight_state(&provider, channel.get())?;
        let mut row = serde_json::to_value(row).unwrap();
        for volatile in ["current_msg_len", "save_generation", "updated_at"] {
            row.as_object_mut().unwrap().remove(volatile);
        }
        Some(row)
    };
    let restored: Vec<_> = cases.iter().map(|case| stored(case.0)).collect();
    assert!(
        restored.iter().all(Option::is_some),
        "restore keeps both rows"
    );
    // Swapped in place: replacing a guard would clear the new answer on the old one's drop.
    for (_, name, after, ..) in &cases {
        inject_liveness(HostSessionRef::tmux(name), Some(*after));
    }
    std::fs::write(tmux.path().join("dead"), "").unwrap();
    let _ = std::fs::remove_file(tmux.path().join("calls"));
    let (channel, name, ..) = &cases[0];
    let polled = format!("list-panes -t ={name}:");
    let read_dead = || {
        let calls = std::fs::read_to_string(tmux.path().join("calls")).unwrap_or_default();
        calls.contains(&polled)
    };
    let handed_off = || stored(cases[1].0) != restored[1];
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !(handed_off() && read_dead()) && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    assert!(handed_off(), "a live pane's handoff moves its row");
    assert!(read_dead(), "the reader read the unobserved pane dead");
    // Past the pane re-check's retries, which a handoff or a retry would follow.
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    assert_eq!(
        stored(*channel),
        restored[0],
        "an unobserved pane keeps the row as stored"
    );
    assert!(!shared.tmux_watchers.has_live_watcher_handle(name));
    drop((cases, transcripts));
    pool.close().await;
    db.drop().await;
}
