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

/// A PATH-first tmux whose sessions exist and whose panes read dead once `dead` exists;
/// every other command succeeds quietly so a claimed watcher stays up.
fn pane_flag_tmux() -> (tempfile::TempDir, crate::config::TestEnvVarGuard) {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let binary = dir.path().join("tmux");
    let body = "#!/bin/sh\n[ \"$1\" = -u ] && shift\nd=\"$(dirname \"$0\")\"\n\
                echo \"$*\" >> \"$d/calls\"\ncase \"$1\" in\n\
                list-panes) if [ -f \"$d/dead\" ]; then echo 1; else echo 0; fi ;;\n\
                capture-pane) echo pane ;;\nesac\nexit 0\n";
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

/// What happens to a restored reader's session after its first unobserved death.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Then {
    /// Stays unobserved and later reads a result.
    Unobserved,
    /// Reads live at the next death check, with no result.
    LiveAtOnce,
    /// Reads live before its result arrives.
    LiveBeforeResult,
    /// Its result is held while the host fails once, then reads live.
    FailedThenLive,
    /// Its result is held while the host fails once, then reads missing.
    FailedThenMissing,
    /// Reads live, then the turn is cancelled.
    CancelledLive,
    /// The turn is cancelled while the host stays unobserved.
    CancelledUnobserved,
    /// Its result is held while the host fails, the turn is stopped, then reads live.
    StoppedWhileHeld,
}

impl Then {
    const ALL: [Self; 8] = [
        Self::Unobserved,
        Self::LiveAtOnce,
        Self::LiveBeforeResult,
        Self::FailedThenLive,
        Self::FailedThenMissing,
        Self::CancelledLive,
        Self::CancelledUnobserved,
        Self::StoppedWhileHeld,
    ];

    fn has_result(self) -> bool {
        !matches!(
            self,
            Self::LiveAtOnce | Self::CancelledLive | Self::CancelledUnobserved
        )
    }

    fn settles(self) -> bool {
        matches!(
            self,
            Self::FailedThenLive | Self::FailedThenMissing | Self::StoppedWhileHeld
        )
    }

    /// The reader's end, RuntimeReady sent, handoff taken by the bridge, result posts.
    fn expected(self) -> (&'static str, usize, bool, Option<usize>) {
        match self {
            Self::Unobserved | Self::FailedThenMissing => ("ended", 0, false, Some(1)),
            Self::LiveAtOnce => ("handoff", 1, true, None),
            Self::LiveBeforeResult | Self::FailedThenLive => ("handoff", 1, true, Some(1)),
            Self::CancelledLive | Self::CancelledUnobserved => ("ended", 0, false, None),
            Self::StoppedWhileHeld => ("handoff", 1, false, Some(0)),
        }
    }
}

/// Whether a watcher handoff stamped the row's input path, which restore never does, and
/// how many watcher claims on its session were seen; a standby claim is retired at once.
fn watch_handoffs(
    shared: Arc<SharedData>,
    provider: ProviderKind,
    channel: ChannelId,
    name: String,
    stop: Arc<std::sync::atomic::AtomicBool>,
) -> std::thread::JoinHandle<(bool, usize)> {
    std::thread::spawn(move || {
        let (mut stamped, mut claims) = (false, Vec::new());
        while !stop.load(std::sync::atomic::Ordering::Relaxed) {
            let row = inflight::load_inflight_state(&provider, channel.get());
            stamped |= row.is_some_and(|row| row.input_fifo_path.is_some());
            if let Some(handle) = shared.tmux_watchers.get(&channel)
                && handle.tmux_session_name == name
            {
                let claim = Arc::as_ptr(&handle.cancel) as usize;
                if !claims.contains(&claim) {
                    claims.push(claim);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        (stamped, claims.len())
    })
}

/// Waits for `reached`, polling; a step never reached fails the test.
async fn reach(what: &str, mut reached: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(45);
    while !reached() {
        assert!(
            std::time::Instant::now() < deadline,
            "never reached: {what}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

// After a restored reader's session goes unobserved, a terminal waits for the host: only a
// confirmed live pane is handed off, right behind the terminal, and a cancel hands off nothing.
#[tokio::test]
async fn a_restored_reader_hands_off_or_retries_only_on_a_confirmed_pane_pg() {
    use super::super::tmux_probe::reader_trace::{self, Event};
    use crate::services::provider::session_probe::SessionLiveness;
    use HostLiveness::{DeadOrAbsent, Live, ProbeError};
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _legacy_output = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let (tmux, _path) = pane_flag_tmux();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let shared = shared_on(&pool).await;
    let provider = ProviderKind::Claude;
    let transcripts = tempfile::tempdir().unwrap();
    let mut cases = Vec::new();
    for (n, then) in Then::ALL.into_iter().enumerate() {
        let channel = ChannelId::new(1_479_671_301_387_160_000 + n as u64);
        let name = provider.build_tmux_session_name(&format!("p5c-reader-{n}"));
        let key = channel_key(&shared, &name);
        seed(&pool, &key, &name, channel.get(), Stored::Legacy).await;
        let session = HostSessionRef::tmux(&name);
        let pane = InjectedLivenessGuard::set(session, DeadOrAbsent);
        let presence = InjectedPresenceGuard::set(session, HostPresence::Present);
        let transcript = transcripts.path().join(format!("{n}.jsonl"));
        std::fs::write(&transcript, "").unwrap();
        // A restart leaves the row with no live mailbox turn, so the recovery kicks off.
        let user_msg = channel.get() + 1;
        let text = "restored reader fixture".to_string();
        let mut row = inflight::InflightTurnState::new(
            provider.clone(),
            channel.get(),
            None,
            1,
            user_msg,
            user_msg + 1,
            text,
            None,
            Some(name.clone()),
            None,
            None,
            0,
        );
        row.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
        row.output_path = Some(transcript.display().to_string());
        inflight::save_inflight_state(&row).unwrap();
        cases.push((then, channel, name, transcript, pane, presence));
    }
    let discord = o_cut_recorder::start(cases[0].1.get()).await;
    let set = |name: &str, answer| inject_liveness(HostSessionRef::tmux(name), Some(answer));
    let unobserved = |name: &str| {
        let deaths = reader_trace::events(name).into_iter().filter(|event| {
            *event
                == Event::Observed {
                    settling: false,
                    liveness: SessionLiveness::ProbeFailed,
                }
        });
        deaths.count() >= 3
    };
    let ended = |name: &str| {
        let events = reader_trace::events(name);
        events.iter().find_map(|event| match event {
            Event::Ended(end) => Some(*end),
            _ => None,
        })
    };
    let sent = |name: &str| {
        let events = reader_trace::events(name);
        events
            .iter()
            .filter(|event| **event == Event::RuntimeReady)
            .count()
    };
    let posts = |result: &str| {
        discord
            .contents()
            .iter()
            .filter(|c| c.contains(result))
            .count()
    };

    restore_inflight_turns(&discord.http, &shared, &provider).await;

    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let watched: Vec<_> = cases
        .iter()
        .map(|(_, channel, name, ..)| {
            let (shared, stop) = (shared.clone(), stop.clone());
            watch_handoffs(shared, provider.clone(), *channel, name.clone(), stop)
        })
        .collect();
    // Every pane reads dead; the host answers live for one session and fails for the rest.
    for (then, _, name, ..) in &cases {
        set(
            name,
            if *then == Then::LiveAtOnce {
                Live
            } else {
                ProbeError
            },
        );
    }
    std::fs::write(tmux.path().join("dead"), "").unwrap();
    for (then, _, name, ..) in &cases {
        match then {
            Then::LiveAtOnce => reach(name, || ended(name) == Some("handoff")).await,
            _ => reach(name, || unobserved(name)).await,
        }
    }
    // Panes read live again; a reader seen polling again has no death check in flight.
    let _ = std::fs::remove_file(tmux.path().join("calls"));
    std::fs::remove_file(tmux.path().join("dead")).unwrap();
    for (then, _, name, ..) in &cases {
        if *then != Then::LiveAtOnce {
            let polled = format!("list-panes -t ={name}:");
            let calls = || std::fs::read_to_string(tmux.path().join("calls")).unwrap_or_default();
            reach(name, || calls().contains(&polled)).await;
        }
    }
    for (then, channel, name, transcript, ..) in &cases {
        if matches!(then, Then::LiveBeforeResult | Then::CancelledLive) {
            set(name, Live);
        }
        if matches!(then, Then::CancelledLive | Then::CancelledUnobserved) {
            let cancel = super::super::mailbox_snapshot(&shared, *channel)
                .await
                .cancel_token;
            let cancel = cancel.expect("the restored turn holds its token");
            cancel
                .cancelled
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
        if then.has_result() {
            let record = format!(
                "{{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"p5c-result-{name}\"}}\n"
            );
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(transcript)
                .unwrap();
            std::io::Write::write_all(&mut file, record.as_bytes()).unwrap();
        }
    }
    // A held result's first settle answer fails; the second is the case's own.
    for (then, channel, name, ..) in &cases {
        if then.settles() {
            let held = Event::Observed {
                settling: true,
                liveness: SessionLiveness::ProbeFailed,
            };
            reach(name, || reader_trace::events(name).contains(&held)).await;
            if *then == Then::StoppedWhileHeld {
                let cancel = super::super::mailbox_snapshot(&shared, *channel)
                    .await
                    .cancel_token;
                let cancel = cancel.expect("the restored turn holds its token");
                cancel
                    .cancelled
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
            set(
                name,
                if *then == Then::FailedThenMissing {
                    DeadOrAbsent
                } else {
                    Live
                },
            );
        }
    }
    for (then, _, name, ..) in &cases {
        reach(name, || ended(name).is_some()).await;
        let result = format!("p5c-result-{name}");
        if then.expected().3 == Some(1) {
            reach(&result, || posts(&result) >= 1).await;
        }
    }
    // Past the bridge's residual window, so a handoff sent would have been taken.
    let settled = std::time::Instant::now();
    reach("residual window", || {
        settled.elapsed() > std::time::Duration::from_secs(2)
    })
    .await;
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let mut wrong = Vec::new();
    for ((then, _, name, ..), watched) in cases.iter().zip(watched) {
        let (end, ready, taken, result_posts) = then.expected();
        let (stamped, claims) = watched.join().unwrap();
        let posted = result_posts.map(|_| posts(&format!("p5c-result-{name}")));
        let seen = (ended(name), sent(name), stamped, posted);
        if seen != (Some(end), ready, taken, result_posts) || claims > usize::from(taken) {
            wrong.push(format!(
                "{then:?}: (end, RuntimeReady sent, taken by the bridge, result posts) {seen:?}, \
                 watcher claims {claims}"
            ));
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
    drop((cases, transcripts));
    pool.close().await;
    db.drop().await;
}
