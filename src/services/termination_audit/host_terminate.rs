//! Host-boundary termination entry. No host supports it yet, so every call is
//! refused before the host is asked anything but its kind.

use crate::services::session_host::{
    HostMutation, HostRefusal, HostedRuntimeLocator, InteractiveSessionHost,
};

/// Operation name carried by the `Unsupported` refusal.
pub(crate) const TERMINATE_OP: &str = "terminate";

/// The execution-identity owner's comparison for the target, as it returned it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IdentityVerdict {
    Match,
    Mismatch,
    Unknown,
}

/// The delivery/cancel owner's answer for the target's current turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StopVerdict {
    DeliveryFencePermits,
    CancelCommitted,
    Withheld,
}

/// Proof that the existing owners cleared this exact target. Not a lease or a
/// new authority: it only carries their verdicts and is consumed by one call.
#[derive(Debug)]
pub(crate) struct HostTerminateWarrant {
    target: HostedRuntimeLocator,
}

impl HostTerminateWarrant {
    /// Only an identity `Match` with a permitting stop verdict yields a warrant.
    pub(super) fn issue(
        target: HostedRuntimeLocator,
        identity: IdentityVerdict,
        stop: StopVerdict,
    ) -> Option<Self> {
        let cleared = identity == IdentityVerdict::Match
            && matches!(
                stop,
                StopVerdict::DeliveryFencePermits | StopVerdict::CancelCommitted
            );
        cleared.then_some(Self { target })
    }
}

/// Terminate the warrant's target on `host`. A warrant for another host kind is
/// a precondition refusal; every current host answers `Unsupported`.
pub(crate) fn terminate_hosted_session(
    host: &dyn InteractiveSessionHost,
    warrant: HostTerminateWarrant,
) -> HostMutation {
    let kind = warrant.target.host_kind;
    if host.kind() != kind {
        return HostMutation::Refused(HostRefusal::Precondition(format!(
            "warrant targets a {} session, host is {}",
            kind.as_str(),
            host.kind().as_str()
        )));
    }
    HostMutation::Refused(HostRefusal::Unsupported {
        kind,
        op: TERMINATE_OP,
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::services::session_host::{
        HostCapabilities, HostError, HostKind, HostLiveness, HostPresence, HostSessionRef, host_for,
    };

    const KINDS: [HostKind; 3] = [HostKind::Tmux, HostKind::Process, HostKind::Herdr];

    fn target(kind: HostKind) -> HostedRuntimeLocator {
        HostedRuntimeLocator {
            execution_node: None,
            host_kind: kind,
            host_session_id: "AgentDesk-claude-p3".to_string(),
            pane: Some("pane-1".to_string()),
        }
    }

    fn warrant(kind: HostKind) -> HostTerminateWarrant {
        HostTerminateWarrant::issue(
            target(kind),
            IdentityVerdict::Match,
            StopVerdict::CancelCommitted,
        )
        .expect("match with a committed cancel issues a warrant")
    }

    /// Counts every host call except `kind`, so a refusal that reaches for
    /// input, interrupt or capture shows up.
    struct SpyHost {
        kind: HostKind,
        calls: AtomicUsize,
    }

    impl SpyHost {
        fn touch(&self) {
            self.calls.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl InteractiveSessionHost for SpyHost {
        fn kind(&self) -> HostKind {
            self.kind
        }
        fn capabilities(&self) -> HostCapabilities {
            self.touch();
            HostCapabilities::default()
        }
        fn presence(&self, _: HostSessionRef<'_>) -> HostPresence {
            self.touch();
            HostPresence::Present
        }
        fn liveness(&self, _: HostSessionRef<'_>) -> HostLiveness {
            self.touch();
            HostLiveness::Live
        }
        fn send_text(&self, _: HostSessionRef<'_>, _: &str) -> Result<HostMutation, HostError> {
            self.touch();
            Ok(HostMutation::Confirmed)
        }
        fn send_keys(&self, _: HostSessionRef<'_>, _: &[&str]) -> Result<HostMutation, HostError> {
            self.touch();
            Ok(HostMutation::Confirmed)
        }
        fn interrupt(&self, _: HostSessionRef<'_>) -> Result<HostMutation, HostError> {
            self.touch();
            Ok(HostMutation::Confirmed)
        }
        fn capture_screen(&self, _: HostSessionRef<'_>, _: i32) -> Result<String, HostError> {
            self.touch();
            Ok(String::new())
        }
        fn current_working_dir(&self, _: HostSessionRef<'_>) -> Result<Option<PathBuf>, HostError> {
            self.touch();
            Ok(None)
        }
        fn execution_pid(&self, _: HostSessionRef<'_>) -> Result<Option<u32>, HostError> {
            self.touch();
            Ok(None)
        }
    }

    #[test]
    fn host_terminate_refuses_every_current_host_without_touching_it() {
        for kind in KINDS {
            let unsupported = HostMutation::Refused(HostRefusal::Unsupported {
                kind,
                op: TERMINATE_OP,
            });
            assert_eq!(
                terminate_hosted_session(host_for(kind), warrant(kind)),
                unsupported
            );
            let spy = SpyHost {
                kind,
                calls: AtomicUsize::new(0),
            };
            assert_eq!(terminate_hosted_session(&spy, warrant(kind)), unsupported);
            assert_eq!(spy.calls.load(Ordering::SeqCst), 0, "{kind:?}");
        }
    }

    #[test]
    fn host_terminate_refuses_a_warrant_for_another_host_kind() {
        let refusal = terminate_hosted_session(host_for(HostKind::Tmux), warrant(HostKind::Herdr));
        assert!(
            matches!(&refusal, HostMutation::Refused(HostRefusal::Precondition(why)) if why.contains("herdr")),
            "{refusal:?}"
        );
    }

    #[test]
    fn host_terminate_warrant_needs_identity_match_and_a_permitting_stop() {
        use IdentityVerdict::{Match, Mismatch, Unknown};
        use StopVerdict::{CancelCommitted, DeliveryFencePermits, Withheld};
        let cleared = [(Match, DeliveryFencePermits), (Match, CancelCommitted)];
        for identity in [Match, Mismatch, Unknown] {
            for stop in [DeliveryFencePermits, CancelCommitted, Withheld] {
                let issued =
                    HostTerminateWarrant::issue(target(HostKind::Herdr), identity, stop).is_some();
                assert_eq!(
                    issued,
                    cleared.contains(&(identity, stop)),
                    "{identity:?}/{stop:?}"
                );
            }
        }
    }
}
