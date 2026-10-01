use super::*;
use crate::services::provider::session_probe::SessionLiveness;
use crate::services::session_host::HostPresence;

#[cfg(unix)]
fn observe_liveness(name: &str) -> SessionLiveness {
    crate::services::discord::host_liveness::observe_liveness(name, None)
}

#[cfg(unix)]
fn observe_presence(name: &str) -> Option<HostPresence> {
    crate::services::discord::host_liveness::observe_presence(name, None)
}

// No tmux off Unix, as before: every session reads absent.
#[cfg(not(unix))]
fn observe_liveness(_name: &str) -> SessionLiveness {
    SessionLiveness::Missing
}

#[cfg(not(unix))]
fn observe_presence(_name: &str) -> Option<HostPresence> {
    Some(HostPresence::Missing)
}

/// Retry-aware pane liveness for recovery after dcserver restart; the first check can
/// false-negative while tmux initializes. Another host is neither probed nor retried.
pub(super) fn observe_liveness_with_retry(name: &str) -> SessionLiveness {
    let mut liveness = observe_liveness(name);
    for attempt in 1..=2u32 {
        if matches!(liveness, SessionLiveness::Alive | SessionLiveness::Unknown) {
            break;
        }
        std::thread::sleep(recovery_retry_backoff(attempt));
        liveness = observe_liveness(name);
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

/// Legacy bool: false only for a confirmed dead tmux pane.
pub(super) fn tmux_session_alive_with_retry(name: &str) -> bool {
    crate::services::discord::host_liveness::not_dead(observe_liveness_with_retry(name))
}

/// Legacy bool: false only for a confirmed missing tmux session.
pub(super) fn tmux_has_session_with_retry(name: &str) -> bool {
    let mut presence = observe_presence(name);
    for attempt in 1..=2u32 {
        if matches!(presence, None | Some(HostPresence::Present)) {
            break;
        }
        std::thread::sleep(recovery_retry_backoff(attempt));
        presence = observe_presence(name);
        if presence == Some(HostPresence::Present) {
            tracing::info!(
                "  [recovery] tmux session found on retry {} for {}",
                attempt,
                name
            );
        }
    }
    presence != Some(HostPresence::Missing)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::services::session_host::test_support::{
        InjectedLivenessGuard, InjectedPresenceGuard,
    };
    use crate::services::session_host::{HostLiveness, HostSessionRef};

    // Restart recovery reads a session dead or missing only when tmux confirms it; a failed
    // probe never does, and a marker naming another host is neither probed nor retried.
    #[test]
    fn restart_probes_read_dead_only_on_a_confirmed_tmux_answer() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let herdr = "AgentDesk-claude-p4b1-restart-herdr";
        let marker = crate::services::tmux_common::session_temp_path(herdr, "host_kind");
        std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
        std::fs::write(marker, "herdr").unwrap();
        let session = HostSessionRef::tmux(herdr);
        let _pane = InjectedLivenessGuard::set(session, HostLiveness::DeadOrAbsent);
        let _presence = InjectedPresenceGuard::set(session, HostPresence::Missing);
        let started = std::time::Instant::now();
        assert_eq!(observe_liveness_with_retry(herdr), SessionLiveness::Unknown);
        assert!(tmux_session_alive_with_retry(herdr));
        assert!(tmux_has_session_with_retry(herdr));
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_millis(600),
            "{elapsed:?}"
        );

        for (n, (pane, presence, not_dead)) in [
            (HostLiveness::Live, HostPresence::Present, true),
            (HostLiveness::ProbeError, HostPresence::ProbeFailed, true),
            (HostLiveness::DeadOrAbsent, HostPresence::Missing, false),
        ]
        .into_iter()
        .enumerate()
        {
            let name = format!("AgentDesk-claude-p4b1-restart-{n}");
            let session = HostSessionRef::tmux(&name);
            let _pane = InjectedLivenessGuard::set(session, pane);
            let _presence = InjectedPresenceGuard::set(session, presence);
            assert_eq!(tmux_session_alive_with_retry(&name), not_dead, "{pane:?}");
            assert_eq!(tmux_has_session_with_retry(&name), not_dead, "{presence:?}");
        }
    }
}
