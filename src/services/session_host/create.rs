//! Typed create effect. The launch owner keeps its Pending row, permits and pane projection, and
//! reads the host's original reply from the outcome. tmux launches exist on unix only.
#[cfg(unix)]
use std::process::Output;

use super::HostKind;
#[cfg(unix)]
use crate::domain::session_identity::managed::ManagedIdentity;
use crate::services::herdr_launch::clear_epoch::EffectPermit;
use crate::services::herdr_launch::policy::UserDefaultCreateRequest;
use crate::services::herdr_launch::{HerdrCreateOutcome, HerdrCreateRequest, HerdrLaunchHost};

const KIND_MISMATCH: &str = "host create target kind mismatch";

#[cfg(unix)]
pub(super) enum TmuxPlace<'a> {
    NewSession(&'a ManagedIdentity),
    /// The session's dead pane, started again in place; a live pane is refused.
    DeadPane,
}

#[cfg(unix)]
pub(super) struct TmuxCreate<'a> {
    pub(super) session: &'a str,
    pub(super) working_dir: Option<&'a str>,
    pub(super) command: &'a str,
    pub(super) place: TmuxPlace<'a>,
}

pub(super) enum Request<'a> {
    #[cfg(unix)]
    Tmux(TmuxCreate<'a>),
    Herdr(&'a dyn HerdrLaunchHost, &'a HerdrCreateRequest),
    HerdrUserDefault(&'a dyn HerdrLaunchHost, &'a UserDefaultCreateRequest),
}

/// A prepared launch its owner already admitted. Only the tmux adapter builds the tmux forms; a
/// Herdr form borrows the launch's clear-epoch effect permit, so it is held until the create ends.
pub(crate) struct HostCreate<'a>(pub(super) Request<'a>);

impl<'a> HostCreate<'a> {
    pub(crate) fn herdr(
        host: &'a dyn HerdrLaunchHost,
        request: &'a HerdrCreateRequest,
        _held: &'a EffectPermit,
    ) -> Self {
        Self(Request::Herdr(host, request))
    }

    pub(crate) fn herdr_user_default(
        host: &'a dyn HerdrLaunchHost,
        request: &'a UserDefaultCreateRequest,
        _held: &'a EffectPermit,
    ) -> Self {
        Self(Request::HerdrUserDefault(host, request))
    }
}

/// What the host did with a create. A reply is never a bound source; the owner records the pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HostCreation {
    /// Nothing reached the host.
    NotSent(String),
    /// The tmux session or Herdr pane the prepared command was started in.
    Created(String),
    /// The host may have acted without a usable confirmation; nothing is resent.
    Indeterminate(String),
}

enum Evidence {
    #[cfg(unix)]
    Output(Result<Output, String>),
    Herdr(HerdrCreateOutcome),
}

pub(crate) struct CreateOutcome {
    pub(crate) creation: HostCreation,
    evidence: Evidence,
}

impl CreateOutcome {
    #[cfg(unix)]
    fn output(session: &str, result: Result<Output, String>) -> Self {
        let creation = match &result {
            Ok(output) if output.status.success() => HostCreation::Created(session.to_owned()),
            Ok(output) => HostCreation::Indeterminate(
                String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            ),
            Err(error) => HostCreation::Indeterminate(error.clone()),
        };
        Self {
            creation,
            evidence: Evidence::Output(result),
        }
    }

    fn herdr(reply: HerdrCreateOutcome) -> Self {
        let creation = match &reply {
            HerdrCreateOutcome::Created { pane_id } if !pane_id.trim().is_empty() => {
                HostCreation::Created(pane_id.clone())
            }
            HerdrCreateOutcome::Created { .. } => {
                HostCreation::Indeterminate("create reply named no pane".into())
            }
            HerdrCreateOutcome::NotSent(detail) => HostCreation::NotSent(detail.clone()),
            HerdrCreateOutcome::Indeterminate(detail) => {
                HostCreation::Indeterminate(detail.clone())
            }
            HerdrCreateOutcome::CreatedUnconfirmed { pane_id, detail }
            | HerdrCreateOutcome::CreatedCommandNotSent { pane_id, detail } => {
                HostCreation::Indeterminate(format!("pane {pane_id}: {detail}"))
            }
        };
        Self {
            creation,
            evidence: Evidence::Herdr(reply),
        }
    }

    fn mismatch(evidence: Evidence) -> Self {
        Self {
            creation: HostCreation::NotSent(KIND_MISMATCH.into()),
            evidence,
        }
    }

    /// tmux's raw result, so the launch keeps its exit status and stderr handling.
    #[cfg(unix)]
    pub(crate) fn into_output(self) -> Result<Output, String> {
        match self.evidence {
            Evidence::Output(output) => output,
            Evidence::Herdr(_) => {
                Err("host create evidence does not contain command output".into())
            }
        }
    }

    /// Herdr's own reply, including an unconfirmed pane the launch still records.
    pub(crate) fn into_herdr(self) -> HerdrCreateOutcome {
        match self.evidence {
            Evidence::Herdr(reply) => reply,
            // A reply of another host proves nothing was left unsent, so no Pending is retired on it.
            #[cfg(unix)]
            Evidence::Output(_) => {
                HerdrCreateOutcome::Indeterminate("host create evidence is not Herdr".into())
            }
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
thread_local! { static IN_PORT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }

/// Whether this thread is inside the create port, for a fake host to tell a bypassing caller.
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn create_in_effect() -> bool {
    IN_PORT.get()
}

pub(super) fn create(kind: HostKind, request: HostCreate<'_>) -> CreateOutcome {
    #[cfg(any(test, feature = "test-support"))]
    let entered = IN_PORT.replace(true);
    let outcome = match (kind, request.0) {
        #[cfg(unix)]
        (HostKind::Tmux, Request::Tmux(request)) => {
            CreateOutcome::output(request.session, super::tmux_host::create(&request))
        }
        (HostKind::Herdr, Request::Herdr(host, request)) => {
            CreateOutcome::herdr(host.create(request))
        }
        (HostKind::Herdr, Request::HerdrUserDefault(host, request)) => {
            CreateOutcome::herdr(host.create_user_default(request))
        }
        (_, Request::Herdr(..) | Request::HerdrUserDefault(..)) => CreateOutcome::mismatch(
            Evidence::Herdr(HerdrCreateOutcome::NotSent(KIND_MISMATCH.into())),
        ),
        #[cfg(unix)]
        _ => CreateOutcome::mismatch(Evidence::Output(Err(KIND_MISMATCH.into()))),
    };
    #[cfg(any(test, feature = "test-support"))]
    IN_PORT.set(entered);
    tracing::debug!(?kind, creation = ?outcome.creation, "host create effect");
    outcome
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::db::dispatched_sessions::hosted_execution::{ExpectedExecution, tests::owner};
    use crate::services::herdr_launch::{EvidenceProbe, HerdrLaunchEndpoint, clear_epoch};
    use crate::services::session_host::{
        EvidenceGap, InteractiveSessionHost, RestoreResume, ServerWitness, TmuxHost, host_for,
    };

    struct Replying(HerdrCreateOutcome, Mutex<Vec<(HerdrCreateRequest, bool)>>);

    impl HerdrLaunchHost for Replying {
        fn mode(&self, _: &HerdrLaunchEndpoint) -> crate::config::session_hosts::EndpointMode {
            crate::config::session_hosts::EndpointMode::LegacyDedicated
        }
        fn restore_resume(&self, _: &HerdrLaunchEndpoint) -> RestoreResume {
            RestoreResume::On
        }
        fn create(&self, request: &HerdrCreateRequest) -> HerdrCreateOutcome {
            let seen = (request.clone(), create_in_effect());
            self.1.lock().unwrap().push(seen);
            self.0.clone()
        }
        fn launch_evidence(&self, _: &EvidenceProbe) -> Result<ExpectedExecution, EvidenceGap> {
            Err(EvidenceGap::UnknownEndpoint)
        }
    }

    // One admitted request reaches the Herdr host once and unchanged, every reply keeps its own
    // meaning for the launch, and a host of another kind sends nothing.
    #[test]
    fn herdr_create_sends_the_admitted_request_once_and_keeps_every_reply_distinct() {
        let launching = clear_epoch::Launching::for_launch(&owner("1479671301387059404")).unwrap();
        let permit = launching
            .scope()
            .check_epoch()
            .unwrap()
            .begin_effect()
            .unwrap();
        let request = HerdrCreateRequest {
            endpoint: HerdrLaunchEndpoint {
                execution_node: "test-node".into(),
                config_key: "herdr.default".into(),
                socket_addr: "/adk/herdr.sock".into(),
                herdr_session: "agentdesk".into(),
            },
            label: "AgentDesk-claude-hosted#n1".into(),
            cwd: "/tmp".into(),
            command: "bash launch.sh".into(),
            restore_off_witness: ServerWitness::for_test(1),
        };
        let pane = |pane_id: &str| HerdrCreateOutcome::Created {
            pane_id: pane_id.into(),
        };
        let unconfirmed = HerdrCreateOutcome::CreatedUnconfirmed {
            pane_id: "w1:p1".into(),
            detail: "command input lost".into(),
        };
        let uncertain = |detail: &str| HostCreation::Indeterminate(detail.into());
        let cases = [
            (pane("w1:p1"), HostCreation::Created("w1:p1".into())),
            (pane(" "), uncertain("create reply named no pane")),
            (unconfirmed, uncertain("pane w1:p1: command input lost")),
            (
                HerdrCreateOutcome::Indeterminate("reply EOF".into()),
                uncertain("reply EOF"),
            ),
            (
                HerdrCreateOutcome::NotSent("refused".into()),
                HostCreation::NotSent("refused".into()),
            ),
        ];
        for (reply, creation) in cases {
            let host = Replying(reply.clone(), Mutex::default());
            let herdr = host_for(HostKind::Herdr);
            let outcome = herdr.create(HostCreate::herdr(&host, &request, &permit));
            assert_eq!(outcome.creation, creation, "{reply:?}");
            assert_eq!(outcome.into_herdr(), reply);
            assert_eq!(
                *host.1.lock().unwrap(),
                [(request.clone(), true)],
                "{reply:?}"
            );

            let other = TmuxHost.create(HostCreate::herdr(&host, &request, &permit));
            assert_eq!(other.creation, HostCreation::NotSent(KIND_MISMATCH.into()));
            assert_eq!(
                other.into_herdr(),
                HerdrCreateOutcome::NotSent(KIND_MISMATCH.into())
            );
            assert_eq!(
                host.1.lock().unwrap().len(),
                1,
                "{reply:?}: a tmux host sent it"
            );
        }
        assert!(!create_in_effect());
    }
}
