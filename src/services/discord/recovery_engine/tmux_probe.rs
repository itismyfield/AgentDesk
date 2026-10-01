use super::*;
use crate::services::discord::host_liveness;
use crate::services::discord::inflight::{InflightTurnState, KeyedTeardown};
use crate::services::provider::session_probe::SessionLiveness;
use crate::services::session_host::HostPresence;

#[cfg(unix)]
fn observe_liveness(name: &str, row: Option<&InflightTurnState>) -> SessionLiveness {
    host_liveness::observe_liveness(name, row)
}

#[cfg(unix)]
fn observe_presence(name: &str, row: Option<&InflightTurnState>) -> Option<HostPresence> {
    host_liveness::observe_presence(name, row)
}

// No tmux off Unix, as before: every session reads absent.
#[cfg(not(unix))]
fn observe_liveness(_name: &str, _row: Option<&InflightTurnState>) -> SessionLiveness {
    SessionLiveness::Missing
}

#[cfg(not(unix))]
fn observe_presence(_name: &str, _row: Option<&InflightTurnState>) -> Option<HostPresence> {
    Some(HostPresence::Missing)
}

/// Retry-aware pane liveness for recovery after dcserver restart; the first check can
/// false-negative while tmux initializes. Another host is neither probed nor retried.
pub(super) fn observe_liveness_with_retry(name: &str) -> SessionLiveness {
    liveness_with_retry(name, None)
}

fn liveness_with_retry(name: &str, row: Option<&InflightTurnState>) -> SessionLiveness {
    let mut liveness = observe_liveness(name, row);
    #[cfg(test)]
    reader_trace::observed(name, liveness);
    for attempt in 1..=2u32 {
        if matches!(liveness, SessionLiveness::Alive | SessionLiveness::Unknown) {
            break;
        }
        std::thread::sleep(recovery_retry_backoff(attempt));
        liveness = observe_liveness(name, row);
        #[cfg(test)]
        reader_trace::observed(name, liveness);
        if liveness == SessionLiveness::Alive {
            tracing::info!(
                "  [recovery] tmux pane alive on retry {} for {}",
                attempt,
                name
            );
        }
    }
    liveness
}

fn presence_with_retry(name: &str, row: Option<&InflightTurnState>) -> Option<HostPresence> {
    let mut presence = observe_presence(name, row);
    for attempt in 1..=2u32 {
        if matches!(presence, None | Some(HostPresence::Present)) {
            break;
        }
        std::thread::sleep(recovery_retry_backoff(attempt));
        presence = observe_presence(name, row);
        if presence == Some(HostPresence::Present) {
            tracing::info!(
                "  [recovery] tmux session found on retry {} for {}",
                attempt,
                name
            );
        }
    }
    presence
}

/// What restart recovery may do with a row's session. For a Claude row only a local tmux
/// answer moves it; another host, a failed probe or a kept death leaves row and report as stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RestartProbe {
    Alive,
    Missing,
    Defer,
}

impl RestartProbe {
    /// `Some(alive)` to act on, `None` to leave the row for a later pass.
    pub(super) fn alive(self) -> Option<bool> {
        match self {
            Self::Alive => Some(true),
            Self::Missing => Some(false),
            Self::Defer => None,
        }
    }

    fn of_liveness(liveness: SessionLiveness) -> Self {
        match liveness {
            SessionLiveness::Alive => Self::Alive,
            SessionLiveness::Missing => Self::Missing,
            SessionLiveness::Unknown | SessionLiveness::ProbeFailed => Self::Defer,
        }
    }

    fn of_presence(presence: Option<HostPresence>) -> Self {
        match presence {
            Some(HostPresence::Present) => Self::Alive,
            Some(HostPresence::Missing) => Self::Missing,
            Some(HostPresence::ProbeFailed) | None => Self::Defer,
        }
    }

    /// Codex and Qwen keep the name-only probe that reads anything but a death as alive.
    fn legacy(self) -> Self {
        if self == Self::Defer {
            Self::Alive
        } else {
            self
        }
    }
}

fn typed(provider: &ProviderKind) -> bool {
    *provider == ProviderKind::Claude
}

/// The row's pane, retried while tmux settles, with no host-guard read.
fn restart_pane_local(
    provider: &ProviderKind,
    name: &str,
    row: &InflightTurnState,
) -> RestartProbe {
    if !typed(provider) {
        return RestartProbe::of_liveness(liveness_with_retry(name, None)).legacy();
    }
    RestartProbe::of_liveness(liveness_with_retry(name, Some(row)))
}

/// The row's pane for a restart decision; a death goes on only when the host guard admits it.
pub(super) async fn restart_pane(
    shared: &SharedData,
    provider: &ProviderKind,
    row: &InflightTurnState,
    name: Option<&str>,
) -> RestartProbe {
    let Some(name) = name else {
        return RestartProbe::Missing;
    };
    let probe = restart_pane_local(provider, name, row);
    admit_death(shared, provider, row, name, probe, "restore_inflight_pane").await
}

/// The row's session presence for a restart decision, guarded as [`restart_pane`] is.
pub(super) async fn restart_session(
    shared: &SharedData,
    provider: &ProviderKind,
    row: &InflightTurnState,
    name: Option<&str>,
) -> RestartProbe {
    let Some(name) = name else {
        return RestartProbe::Missing;
    };
    if !typed(provider) {
        return RestartProbe::of_presence(presence_with_retry(name, None)).legacy();
    }
    let probe = RestartProbe::of_presence(presence_with_retry(name, Some(row)));
    admit_death(
        shared,
        provider,
        row,
        name,
        probe,
        "restore_inflight_session",
    )
    .await
}

async fn admit_death(
    shared: &SharedData,
    provider: &ProviderKind,
    row: &InflightTurnState,
    name: &str,
    probe: RestartProbe,
    caller: &str,
) -> RestartProbe {
    let verdict = match probe {
        RestartProbe::Missing if typed(provider) => {
            let observed = SessionLiveness::Missing;
            let channel = row.channel_id;
            let gate =
                host_liveness::tmux_verdict_gate(shared, provider, channel, name, observed, caller);
            match gate.await {
                // A turn start's sessions row write is best effort; no row and no trace keeps main.
                KeyedTeardown::Cleared(_) | KeyedTeardown::RowMissing => RestartProbe::Missing,
                KeyedTeardown::Kept => RestartProbe::Defer,
            }
        }
        probe => probe,
    };
    if verdict == RestartProbe::Defer {
        tracing::info!(
            name,
            caller,
            channel_id = row.channel_id,
            "restart recovery deferred: row and report kept"
        );
    }
    verdict
}

/// The restore reader's poll probe: a Claude row whose host evidence is not local tmux
/// polls its transcript without a tmux probe; any other row keeps the legacy tmux probe.
fn reader_probe(
    provider: &ProviderKind,
    row: &InflightTurnState,
    name: &str,
    runtime_kind: RuntimeHandoffKind,
    output_path: &str,
) -> crate::services::provider::SessionProbe {
    let tmux = (!typed(provider) || host_liveness::local_tmux(name, Some(row))).then_some(name);
    let probe = crate::services::claude::host_gate::host_poll_probe;
    probe(tmux, provider.clone(), Some(runtime_kind), output_path)
}

/// The session a restore reader follows: its provider, row, tmux name and output.
pub(super) struct RestoredReader {
    pub(super) provider: ProviderKind,
    pub(super) row: InflightTurnState,
    pub(super) name: String,
    pub(super) runtime_kind: RuntimeHandoffKind,
    pub(super) output_path: String,
}

/// How a restore reader ended: hand the session to a watcher from an offset, retry a turn
/// whose session died, or end a finished turn with no handoff.
pub(super) enum RestoredRead {
    HandOff(u64),
    Died,
    Ended,
}

/// Reads restored output to a result; a death the host cannot confirm keeps reading from
/// the same offset, and from then on a terminal and the frames after it wait for the host.
pub(super) fn read_restored_output(
    reader: &RestoredReader,
    mut offset: u64,
    tx: &std::sync::mpsc::Sender<StreamMessage>,
    cancel: Arc<CancelToken>,
) -> Result<RestoredRead, String> {
    let RestoredReader {
        provider,
        row,
        name,
        ..
    } = reader;
    let mut deferred = false;
    loop {
        let probe = reader_probe(
            provider,
            row,
            name,
            reader.runtime_kind,
            &reader.output_path,
        );
        let read = |sender| {
            crate::services::session_backend::read_output_file_until_result(
                &reader.output_path,
                offset,
                sender,
                Some(cancel.clone()),
                probe,
            )
        };
        // Held terminals go out only after the host is asked, so the bridge gets a terminal
        // and its handoff back to back, as on a read that never deferred.
        let (read, held) = if deferred {
            read_holding_terminal(tx, read)
        } else {
            (read(tx.clone()), Vec::new())
        };
        let step = match read {
            Err(error) => Err(error),
            Ok(ReadOutputResult::Completed { offset } | ReadOutputResult::Cancelled { offset })
                if !deferred =>
            {
                Ok(Some(RestoredRead::HandOff(offset)))
            }
            // The bridge finalizes a cancel before it would take a later handoff.
            Ok(ReadOutputResult::Cancelled { .. }) => Ok(Some(RestoredRead::Ended)),
            Ok(ReadOutputResult::Completed { offset }) => Ok(Some(settle_result(reader, offset))),
            Ok(ReadOutputResult::SessionDied { offset: died_at }) => {
                offset = died_at;
                Ok(after_death(reader, died_at))
            }
        };
        for frame in held {
            let _ = tx.send(frame);
        }
        if let Some(end) = step? {
            return Ok(end);
        }
        deferred = true;
        std::thread::sleep(recovery_retry_backoff(3));
    }
}

/// One read whose frames from its first terminal on are held back, in order, while earlier
/// frames stream on.
fn read_holding_terminal<R>(
    tx: &std::sync::mpsc::Sender<StreamMessage>,
    read: impl FnOnce(std::sync::mpsc::Sender<StreamMessage>) -> R,
) -> (R, Vec<StreamMessage>) {
    use crate::services::discord::turn_bridge::is_done_setting_terminal_frame as terminal;
    let (inner_tx, inner_rx) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        let forward = scope.spawn(move || {
            let mut held = Vec::new();
            for frame in inner_rx {
                if !held.is_empty() || terminal(&frame) {
                    held.push(frame);
                } else {
                    let _ = tx.send(frame);
                }
            }
            held
        });
        let read = read(inner_tx);
        (read, forward.join().unwrap_or_default())
    })
}

/// The answer after a read saw its session die; `None` while the pane stays unobserved.
fn after_death(reader: &RestoredReader, died_at: u64) -> Option<RestoredRead> {
    // dcserver restart can read as a death with no new output while the CLI idles.
    match restart_pane_local(&reader.provider, &reader.name, &reader.row) {
        RestartProbe::Alive => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ↻ Recovery: session idle but pane alive — handing off to watcher (channel {})",
                reader.row.channel_id
            );
            Some(RestoredRead::HandOff(died_at))
        }
        RestartProbe::Missing => Some(RestoredRead::Died),
        RestartProbe::Defer => {
            let name = reader.name.as_str();
            tracing::info!(name, "restore reader: pane unobserved, reading on");
            None
        }
    }
}

/// A result read past an unobserved pane is handed to a watcher only on a confirmed live pane.
fn settle_result(reader: &RestoredReader, offset: u64) -> RestoredRead {
    #[cfg(test)]
    let _settling = reader_trace::Settling::enter();
    match restart_pane_local(&reader.provider, &reader.name, &reader.row) {
        RestartProbe::Alive => RestoredRead::HandOff(offset),
        verdict => {
            let name = reader.name.as_str();
            tracing::info!(name, ?verdict, "restore reader: result kept from a watcher");
            RestoredRead::Ended
        }
    }
}

/// What a restore reader did, for tests: each pane observation (and whether it settled a
/// held result), each RuntimeReady it sent, and how it ended.
#[cfg(test)]
pub(super) mod reader_trace {
    use super::SessionLiveness;
    use std::sync::Mutex;

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(in crate::services::discord) enum Event {
        Observed {
            settling: bool,
            liveness: SessionLiveness,
        },
        RuntimeReady,
        Ended(&'static str),
    }

    static EVENTS: Mutex<Vec<(String, Event)>> = Mutex::new(Vec::new());

    std::thread_local! {
        static SETTLING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    fn push(name: &str, event: Event) {
        let mut events = EVENTS.lock().unwrap_or_else(|poison| poison.into_inner());
        events.push((name.to_string(), event));
    }

    pub(super) fn observed(name: &str, liveness: SessionLiveness) {
        let settling = SETTLING.with(std::cell::Cell::get);
        push(name, Event::Observed { settling, liveness });
    }

    pub(in crate::services::discord) fn runtime_ready(name: &str) {
        push(name, Event::RuntimeReady);
    }

    pub(in crate::services::discord) fn end_of(
        read: &Result<super::RestoredRead, String>,
    ) -> &'static str {
        match read {
            Ok(super::RestoredRead::HandOff(_)) => "handoff",
            Ok(super::RestoredRead::Died) => "died",
            Ok(super::RestoredRead::Ended) => "ended",
            Err(_) => "error",
        }
    }

    /// Recorded once the reader thread has sent everything it will send.
    pub(in crate::services::discord) fn ended(name: &str, end: &'static str) {
        push(name, Event::Ended(end));
    }

    pub(in crate::services::discord) fn events(name: &str) -> Vec<Event> {
        let events = EVENTS.lock().unwrap_or_else(|poison| poison.into_inner());
        let mine = events.iter().filter(|(owner, _)| owner == name);
        mine.map(|(_, event)| event.clone()).collect()
    }

    /// Marks this thread's observations as settling a held result until dropped.
    pub(super) struct Settling;

    impl Settling {
        pub(super) fn enter() -> Self {
            SETTLING.with(|settling| settling.set(true));
            Self
        }
    }

    impl Drop for Settling {
        fn drop(&mut self) {
            SETTLING.with(|settling| settling.set(false));
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::services::session_host::test_support::{
        InjectedLivenessGuard, InjectedPresenceGuard,
    };
    use crate::services::session_host::{HostLiveness, HostSessionRef};

    fn row(provider: ProviderKind, name: &str) -> InflightTurnState {
        let text = "restart probe fixture".to_string();
        let name = Some(name.to_string());
        InflightTurnState::new(
            provider, 5340, None, 1, 2, 3, text, None, name, None, None, 0,
        )
    }

    // A Claude row reads dead only on a confirmed tmux answer; a failed probe, a marker or a
    // row snapshot of another host defers with no probe or retry. Codex keeps main's bool.
    #[test]
    fn restart_probes_read_dead_only_on_a_confirmed_tmux_answer() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let (claude, codex) = (ProviderKind::Claude, ProviderKind::Codex);
        let herdr = "AgentDesk-claude-p4b1-restart-herdr";
        let marker = crate::services::tmux_common::session_temp_path(herdr, "host_kind");
        std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
        std::fs::write(marker, "herdr").unwrap();
        let session = HostSessionRef::tmux(herdr);
        let _pane = InjectedLivenessGuard::set(session, HostLiveness::DeadOrAbsent);
        let _presence = InjectedPresenceGuard::set(session, HostPresence::Missing);
        let started = std::time::Instant::now();
        assert_eq!(observe_liveness_with_retry(herdr), SessionLiveness::Unknown);
        let probe = restart_pane_local(&claude, herdr, &row(claude.clone(), herdr));
        assert_eq!(probe, RestartProbe::Defer);
        assert_eq!(presence_with_retry(herdr, None), None);
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_millis(600),
            "{elapsed:?}"
        );

        let located = "AgentDesk-claude-p5c-restart-located";
        let session = HostSessionRef::tmux(located);
        let _pane = InjectedLivenessGuard::set(session, HostLiveness::DeadOrAbsent);
        let mut hosted = row(claude.clone(), located);
        hosted.runtime_kind_unknown_on_disk = true;
        let probe = restart_pane_local(&claude, located, &hosted);
        assert_eq!(probe, RestartProbe::Defer, "the row snapshot is read");

        for (n, (pane, presence, claude_probe, codex_probe)) in [
            (
                HostLiveness::Live,
                HostPresence::Present,
                RestartProbe::Alive,
                RestartProbe::Alive,
            ),
            (
                HostLiveness::ProbeError,
                HostPresence::ProbeFailed,
                RestartProbe::Defer,
                RestartProbe::Alive,
            ),
            (
                HostLiveness::DeadOrAbsent,
                HostPresence::Missing,
                RestartProbe::Missing,
                RestartProbe::Missing,
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let name = format!("AgentDesk-claude-p4b1-restart-{n}");
            let session = HostSessionRef::tmux(&name);
            let _pane = InjectedLivenessGuard::set(session, pane);
            let _presence = InjectedPresenceGuard::set(session, presence);
            let pane_probe = |provider: &ProviderKind| {
                restart_pane_local(provider, &name, &row(provider.clone(), &name))
            };
            assert_eq!(pane_probe(&claude), claude_probe, "{pane:?}");
            assert_eq!(pane_probe(&codex), codex_probe, "codex {pane:?}");
            let presence_probe = RestartProbe::of_presence(presence_with_retry(&name, None));
            assert_eq!(presence_probe, claude_probe, "{presence:?}");
            assert_eq!(presence_probe.legacy(), codex_probe, "codex {presence:?}");
        }
    }
}
