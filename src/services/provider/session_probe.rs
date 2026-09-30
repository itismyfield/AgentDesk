//! Output-poll session probes: the legacy bool callbacks, and the typed
//! host-aware observation that only a verified tmux answer may collapse.

use super::ProviderKind;
#[cfg(unix)]
use super::tmux_session_fallback_ready_for_input;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::session_host::{
    HostKind, HostKindResolution, HostLiveness, HostSessionRef, host_for,
};

/// Callbacks for session status checks during output file polling.
pub(crate) struct SessionProbe {
    /// Returns true if the session process is still running.
    pub is_alive: Box<dyn Fn() -> bool + Send>,
    /// Returns true if the session is idle and ready for new input.
    pub is_ready_for_input: Box<dyn Fn() -> bool + Send>,
}

impl SessionProbe {
    pub fn new(
        is_alive: impl Fn() -> bool + Send + 'static,
        is_ready_for_input: impl Fn() -> bool + Send + 'static,
    ) -> Self {
        Self {
            is_alive: Box::new(is_alive),
            is_ready_for_input: Box::new(is_ready_for_input),
        }
    }

    #[cfg(unix)]
    pub fn tmux(session_name: String, provider: ProviderKind) -> Self {
        let runtime_kind =
            crate::services::tmux_common::resolve_tmux_runtime_kind_marker(&session_name);
        Self::tmux_with_runtime(session_name, provider, runtime_kind)
    }

    #[cfg(unix)]
    pub fn tmux_with_runtime(
        session_name: String,
        provider: ProviderKind,
        runtime_kind: Option<crate::services::agent_protocol::RuntimeHandoffKind>,
    ) -> Self {
        let name_alive = session_name.clone();
        let name_ready = session_name;
        let provider_ready = provider;
        Self::new(
            move || tmux_session_alive(&name_alive),
            move || {
                tmux_session_fallback_ready_for_input(&name_ready, &provider_ready, runtime_kind)
                    .is_some_and(crate::services::pane_readiness::FallbackPaneReadiness::is_ready)
            },
        )
    }

    #[cfg(unix)]
    pub fn tmux_with_structured_output(
        session_name: String,
        provider: ProviderKind,
        runtime_kind: Option<crate::services::agent_protocol::RuntimeHandoffKind>,
        output_path: String,
    ) -> Self {
        let name_alive = session_name.clone();
        let name_ready = session_name;
        let provider_ready = provider;
        Self::new(
            move || tmux_session_alive(&name_alive),
            move || {
                crate::services::tui_turn_state::jsonl_ready_for_input(
                    &provider_ready,
                    runtime_kind,
                    std::path::Path::new(&output_path),
                    None,
                )
                .map(crate::services::tui_turn_state::TuiReadyState::is_ready)
                .or_else(|| {
                    tmux_session_fallback_ready_for_input(
                        &name_ready,
                        &provider_ready,
                        runtime_kind,
                    )
                    .map(crate::services::pane_readiness::FallbackPaneReadiness::is_ready)
                })
                .unwrap_or(false)
            },
        )
    }

    #[cfg(not(unix))]
    pub fn tmux(_session_name: String, _provider: ProviderKind) -> Self {
        Self::new(|| false, || false)
    }

    #[cfg(not(unix))]
    pub fn tmux_with_runtime(
        _session_name: String,
        _provider: ProviderKind,
        _runtime_kind: Option<crate::services::agent_protocol::RuntimeHandoffKind>,
    ) -> Self {
        Self::new(|| false, || false)
    }

    #[cfg(not(unix))]
    pub fn tmux_with_structured_output(
        _session_name: String,
        _provider: ProviderKind,
        _runtime_kind: Option<crate::services::agent_protocol::RuntimeHandoffKind>,
        _output_path: String,
    ) -> Self {
        Self::new(|| false, || false)
    }

    pub fn process(is_alive: impl Fn() -> bool + Send + 'static) -> Self {
        Self::new(is_alive, || false)
    }
}

#[cfg(unix)]
fn tmux_session_alive(tmux_session_name: &str) -> bool {
    crate::services::tmux_diagnostics::tmux_session_has_live_pane(tmux_session_name)
}

/// Host-aware liveness. Only `Missing` is a death verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionLiveness {
    Alive,
    Missing,
    /// Unresolved host, or a host this build cannot observe.
    Unknown,
    ProbeFailed,
}

impl From<HostLiveness> for SessionLiveness {
    fn from(value: HostLiveness) -> Self {
        match value {
            HostLiveness::Live => Self::Alive,
            HostLiveness::DeadOrAbsent => Self::Missing,
            HostLiveness::ProbeError => Self::ProbeFailed,
        }
    }
}

/// A liveness answer and the host that gave it (`None`: host unresolved).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SessionObservation {
    pub host: Option<HostKind>,
    pub liveness: SessionLiveness,
}

impl SessionObservation {
    /// Legacy bool: only tmux `Alive`/`Missing` collapse; the rest is `None`,
    /// never a dead session.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn legacy_tmux_alive(self) -> Option<bool> {
        match (self.host, self.liveness) {
            (Some(HostKind::Tmux), SessionLiveness::Alive) => Some(true),
            (Some(HostKind::Tmux), SessionLiveness::Missing) => Some(false),
            _ => None,
        }
    }
}

/// The session a probe looks at, taken from resolved host evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SessionProbeTarget {
    Tmux(String),
    Process(String),
    /// No Herdr observer exists yet; a pane id is never taken from the name.
    Herdr,
    /// Unresolved or conflicting host evidence.
    Unknown,
}

impl SessionProbeTarget {
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn from_resolution(session_name: &str, resolution: HostKindResolution) -> Self {
        let name = session_name.to_string();
        match resolution {
            HostKindResolution::Known { kind, .. } => match kind {
                HostKind::Tmux => Self::Tmux(name),
                HostKind::Process => Self::Process(name),
                HostKind::Herdr => Self::Herdr,
            },
            HostKindResolution::Unknown | HostKindResolution::Conflict { .. } => Self::Unknown,
        }
    }

    pub(crate) fn host_kind(&self) -> Option<HostKind> {
        match self {
            Self::Tmux(_) => Some(HostKind::Tmux),
            Self::Process(_) => Some(HostKind::Process),
            Self::Herdr => Some(HostKind::Herdr),
            Self::Unknown => None,
        }
    }

    fn host_ref(&self) -> Option<HostSessionRef<'_>> {
        match self {
            Self::Tmux(name) => Some(HostSessionRef::tmux(name)),
            Self::Process(name) => Some(HostSessionRef::process(name)),
            Self::Herdr | Self::Unknown => None,
        }
    }
}

/// Typed liveness through the host adapters. Herdr has no observer here and
/// reads `Unknown`; it never reaches a tmux probe.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn observe_session_liveness(target: &SessionProbeTarget) -> SessionObservation {
    let liveness = target
        .host_ref()
        .map_or(SessionLiveness::Unknown, observe_host_liveness);
    SessionObservation {
        host: target.host_kind(),
        liveness,
    }
}

fn observe_host_liveness(session: HostSessionRef<'_>) -> SessionLiveness {
    #[cfg(test)]
    if let Some(injected) = crate::services::session_host::test_support::injected_liveness(session)
    {
        return injected.into();
    }
    host_for(session.kind).liveness(session).into()
}

/// Why no bool probe was built for a target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SessionProbeRefusal {
    /// The provider keeps its legacy tmux alias only.
    LegacyTmuxOnly(ProviderKind),
    /// No bool probe exists for this host; the caller defers instead of
    /// assuming death.
    Unobservable(Option<HostKind>),
}

impl SessionProbe {
    /// Common entry: bool callbacks only for a verified tmux or process target.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn for_target(
        target: &SessionProbeTarget,
        provider: ProviderKind,
        runtime_kind: Option<RuntimeHandoffKind>,
    ) -> Result<Self, SessionProbeRefusal> {
        if provider == ProviderKind::Qwen && !matches!(target, SessionProbeTarget::Tmux(_)) {
            return Err(SessionProbeRefusal::LegacyTmuxOnly(provider));
        }
        match target {
            SessionProbeTarget::Tmux(name) => Ok(Self::tmux_with_runtime(
                name.clone(),
                provider,
                runtime_kind,
            )),
            SessionProbeTarget::Process(name) => Ok(
                crate::services::session_backend::process_session_probe(name),
            ),
            SessionProbeTarget::Herdr | SessionProbeTarget::Unknown => {
                Err(SessionProbeRefusal::Unobservable(target.host_kind()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::platform::tmux::PaneLiveness;
    use crate::services::session_host::test_support::{InjectedLivenessGuard, injected_liveness};
    use crate::services::session_host::{HostKindSource, HostLiveness, HostSessionRef};
    use crate::services::tmux_diagnostics::{
        PaneLivenessOverrideGuard, probe_tmux_session_pane_liveness,
    };

    fn tmux(name: &str) -> SessionProbeTarget {
        SessionProbeTarget::Tmux(name.to_string())
    }

    fn resolved_herdr(name: &str) -> SessionProbeTarget {
        let resolution = HostKindResolution::Known {
            kind: HostKind::Herdr,
            source: HostKindSource::DurableRuntimeKind,
        };
        SessionProbeTarget::from_resolution(name, resolution)
    }

    #[test]
    fn tmux_alive_and_missing_collapse_like_the_legacy_bool() {
        for (injected, liveness, legacy) in [
            (HostLiveness::Live, SessionLiveness::Alive, true),
            (HostLiveness::DeadOrAbsent, SessionLiveness::Missing, false),
        ] {
            let name = "session-probe-tmux-collapse";
            let _guard = InjectedLivenessGuard::set(HostSessionRef::tmux(name), injected);
            let observed = observe_session_liveness(&tmux(name));
            assert_eq!(observed.host, Some(HostKind::Tmux));
            assert_eq!(observed.liveness, liveness);
            assert_eq!(observed.legacy_tmux_alive(), Some(legacy));
        }
        // Real probes: a blank name short-circuits without spawning tmux.
        let blank = observe_session_liveness(&tmux(""));
        let legacy = SessionProbe::tmux_with_runtime(String::new(), ProviderKind::Claude, None);
        assert_eq!(blank.legacy_tmux_alive(), Some((legacy.is_alive)()));
    }

    #[test]
    fn unresolved_herdr_and_failed_probes_never_read_as_dead() {
        // A same-named tmux session that reads dead must not answer for Herdr.
        let name = "session-probe-herdr";
        let _tmux =
            InjectedLivenessGuard::set(HostSessionRef::tmux(name), HostLiveness::DeadOrAbsent);
        let _failed = InjectedLivenessGuard::set(
            HostSessionRef::tmux("session-probe-failed"),
            HostLiveness::ProbeError,
        );
        for (target, host, liveness) in [
            (
                resolved_herdr(name),
                Some(HostKind::Herdr),
                SessionLiveness::Unknown,
            ),
            (
                SessionProbeTarget::from_resolution(name, HostKindResolution::Unknown),
                None,
                SessionLiveness::Unknown,
            ),
            (
                tmux("session-probe-failed"),
                Some(HostKind::Tmux),
                SessionLiveness::ProbeFailed,
            ),
        ] {
            let observed = observe_session_liveness(&target);
            assert_eq!(
                (observed.host, observed.liveness),
                (host, liveness),
                "{target:?}"
            );
            assert_eq!(observed.legacy_tmux_alive(), None, "{target:?}");
        }
        for target in [resolved_herdr(name), SessionProbeTarget::Unknown] {
            let refused = SessionProbe::for_target(&target, ProviderKind::Claude, None).err();
            assert_eq!(
                refused,
                Some(SessionProbeRefusal::Unobservable(target.host_kind()))
            );
        }
    }

    #[tokio::test]
    async fn legacy_pane_override_and_typed_probe_share_one_answer() {
        let legacy = "session-probe-legacy-override";
        let injected = "session-probe-injected";
        {
            let _legacy = PaneLivenessOverrideGuard::set(legacy, PaneLiveness::Live);
            let _injected = InjectedLivenessGuard::set(
                HostSessionRef::tmux(injected),
                HostLiveness::ProbeError,
            );
            assert_eq!(
                observe_session_liveness(&tmux(legacy)).liveness,
                SessionLiveness::Alive
            );
            assert_eq!(
                probe_tmux_session_pane_liveness(legacy).await,
                PaneLiveness::Live
            );
            assert_eq!(
                probe_tmux_session_pane_liveness(injected).await,
                PaneLiveness::ProbeError
            );
            // The tmux answer is keyed by host kind and never reaches the pane.
            assert_eq!(
                observe_session_liveness(&resolved_herdr(legacy)).liveness,
                SessionLiveness::Unknown
            );
        }
        assert_eq!(injected_liveness(HostSessionRef::tmux(legacy)), None);
        assert_eq!(injected_liveness(HostSessionRef::tmux(injected)), None);
    }

    #[test]
    fn qwen_common_probe_keeps_the_legacy_tmux_alias_only() {
        let refused = [
            resolved_herdr("session-probe-qwen"),
            SessionProbeTarget::Unknown,
            SessionProbeTarget::Process("session-probe-qwen".to_string()),
        ];
        for target in refused {
            let result = SessionProbe::for_target(&target, ProviderKind::Qwen, None).err();
            assert_eq!(
                result,
                Some(SessionProbeRefusal::LegacyTmuxOnly(ProviderKind::Qwen)),
                "{target:?}"
            );
        }
        assert!(SessionProbe::for_target(&tmux(""), ProviderKind::Qwen, None).is_ok());
    }
}
