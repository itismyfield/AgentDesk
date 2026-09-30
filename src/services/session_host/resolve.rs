// The session target resolver has no production caller until its consumers land.
#![cfg_attr(not(test), allow(dead_code))]

use super::herdr_host::UnconfiguredHerdrHost;
use super::model::{HostKind, HostKindResolution, HostKindSource, HostSessionRef};
use super::process_host::ProcessHost;
use super::tmux_host::TmuxHost;
use super::traits::InteractiveSessionHost;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::discord::session_identity::tmux_name_from_session_key;
use crate::services::tmux_common::host_marker::{HostKindMarker, read_host_kind_marker};

/// Host-kind evidence the caller already read at its own site. The resolver
/// performs no lookups, so each site keeps its probe order and timeouts.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct HostEvidence<'a> {
    pub session_name: Option<&'a str>,
    /// Turn-bound `runtime_kind` (inflight row or cancel token).
    pub durable_runtime_kind: Option<RuntimeHandoffKind>,
    /// `tmux_common::resolve_tmux_runtime_kind_marker(name)` as read by the caller.
    pub runtime_kind_marker: Option<RuntimeHandoffKind>,
    /// `session_backend::process_session_pid(name).is_some()` as read by the caller.
    pub process_registry_hit: bool,
}

/// Pure: votes in fixed evidence order; disagreement reports the first vote and
/// the first vote that differs from it.
pub(crate) fn resolve_host_kind(evidence: HostEvidence<'_>) -> HostKindResolution {
    if !evidence
        .session_name
        .is_some_and(|name| !name.trim().is_empty())
    {
        return HostKindResolution::Unknown;
    }
    let mut votes: Vec<(HostKind, HostKindSource)> = Vec::with_capacity(3);
    if let Some(kind) = evidence.durable_runtime_kind {
        votes.push((kind_hint(kind), HostKindSource::DurableRuntimeKind));
    }
    if let Some(kind) = evidence.runtime_kind_marker {
        votes.push((kind_hint(kind), HostKindSource::RuntimeKindMarker));
    }
    if evidence.process_registry_hit {
        votes.push((HostKind::Process, HostKindSource::ProcessRegistry));
    }
    let Some(&first) = votes.first() else {
        return HostKindResolution::Unknown;
    };
    match votes.iter().copied().find(|(kind, _)| *kind != first.0) {
        None => HostKindResolution::Known {
            kind: first.0,
            source: first.1,
        },
        Some(second) => HostKindResolution::Conflict { first, second },
    }
}

// No wildcard arm: a new runtime kind must choose its host here.
fn kind_hint(kind: RuntimeHandoffKind) -> HostKind {
    match kind {
        RuntimeHandoffKind::ProcessBackend | RuntimeHandoffKind::ClaudeEAdapter => {
            HostKind::Process
        }
        RuntimeHandoffKind::LegacyTmuxWrapper
        | RuntimeHandoffKind::ClaudeTui
        | RuntimeHandoffKind::CodexTui => HostKind::Tmux,
    }
}

/// How a consumer names a session. The full key is kept as given: no host or
/// name is ever cut from its `:` suffix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SessionTargetInput {
    SessionKey(String),
    Canonical {
        provider: String,
        token_hash: String,
        channel_id: u64,
    },
    /// Raw name from a legacy compatibility API; it needs legacy evidence to reach tmux.
    RawName(String),
}

/// One host-location witness as its reader saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HostWitness {
    /// The reader confirmed nothing is recorded.
    Absent,
    /// `target` is the host session id (pane id for Herdr) when the witness names one.
    Known {
        kind: HostKind,
        target: Option<String>,
    },
    /// Present but unparsable, truncated or naming a host this binary does not know.
    Unrecognized(String),
    /// Not read, or the read failed; never taken as absent.
    ReadFailed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TargetSource {
    SessionRecord,
    InflightLocator,
    HostMarker,
    Legacy(HostKindSource),
}

/// Evidence an injected source read for one target. Start from `unread()` so a
/// witness the source skipped stays fail-closed instead of reading as absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionTargetEvidence {
    /// Full key of the matched sessions row.
    pub session_key: Option<String>,
    /// Name the row records for a tmux/process host; never used for Herdr.
    pub session_name: Option<String>,
    pub session_record: HostWitness,
    pub inflight_locator: HostWitness,
    pub host_marker: HostWitness,
    /// Turn-bound kind the caller read itself, such as a cancel token's.
    pub durable_runtime_kind: Option<RuntimeHandoffKind>,
    /// Kind the inflight row stores; a separate vote, so a disagreement stays visible.
    pub inflight_runtime_kind: Option<RuntimeHandoffKind>,
    /// A runtime kind was stored but this binary does not know it.
    pub runtime_kind_unrecognized: bool,
    pub runtime_kind_marker: Option<RuntimeHandoffKind>,
    pub process_registry_hit: bool,
}

impl SessionTargetEvidence {
    pub(crate) fn unread() -> Self {
        let unread = || HostWitness::ReadFailed("not read".to_string());
        Self {
            session_key: None,
            session_name: None,
            session_record: unread(),
            inflight_locator: unread(),
            host_marker: unread(),
            durable_runtime_kind: None,
            inflight_runtime_kind: None,
            runtime_kind_unrecognized: false,
            runtime_kind_marker: None,
            process_registry_hit: false,
        }
    }

    /// Reads the `.host_kind` marker under the matched key's tmux name, the name the
    /// launch writes it under and session cleanup reads it by. Without a key it stays unread.
    pub(crate) fn with_host_marker(mut self) -> Self {
        let Some(key) = self.session_key.as_deref() else {
            return self;
        };
        self.host_marker = match tmux_name_from_session_key(key) {
            None => HostWitness::Absent,
            Some(name) => match read_host_kind_marker(&name) {
                HostKindMarker::Absent => HostWitness::Absent,
                HostKindMarker::Known(kind) => HostWitness::Known {
                    kind,
                    target: matches!(kind, HostKind::Tmux | HostKind::Process).then_some(name),
                },
                HostKindMarker::Unrecognized(raw) => HostWitness::Unrecognized(raw),
                HostKindMarker::ReadFailed(error) => HostWitness::ReadFailed(error),
            },
        };
        self
    }
}

/// Reads host evidence for a target; the sessions-row adapter and test fakes implement it.
pub(crate) trait SessionTargetEvidenceSource {
    fn read_evidence(&self, input: &SessionTargetInput) -> SessionTargetEvidence;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UnknownHost {
    NoHostEvidence,
    Unreadable {
        source: TargetSource,
        detail: String,
    },
    /// The host is known but no witness names the session on it.
    MissingTarget(HostKind),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TargetHost {
    Known {
        kind: HostKind,
        source: TargetSource,
        name: String,
    },
    Unknown(UnknownHost),
    /// Two witnesses disagree on the host or on the session it names.
    Conflict {
        first: (HostKind, TargetSource),
        second: (HostKind, TargetSource),
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedSessionTarget {
    pub input: SessionTargetInput,
    pub session_key: Option<String>,
    pub host: TargetHost,
}

impl ResolvedSessionTarget {
    /// A tmux/process ref for a known legacy host; Herdr, Unknown and Conflict get none.
    pub(crate) fn legacy_ref(&self) -> Option<HostSessionRef<'_>> {
        match &self.host {
            TargetHost::Known {
                kind: HostKind::Tmux,
                name,
                ..
            } => Some(HostSessionRef::tmux(name)),
            TargetHost::Known {
                kind: HostKind::Process,
                name,
                ..
            } => Some(HostSessionRef::process(name)),
            _ => None,
        }
    }
}

pub(crate) fn resolve_session_target(
    input: SessionTargetInput,
    source: &dyn SessionTargetEvidenceSource,
) -> ResolvedSessionTarget {
    let evidence = source.read_evidence(&input);
    ResolvedSessionTarget {
        input,
        host: resolve_target_host(&evidence),
        session_key: evidence.session_key,
    }
}

/// Explicit witnesses decide first; only a row with none of them keeps the
/// legacy runtime-kind reading.
fn resolve_target_host(evidence: &SessionTargetEvidence) -> TargetHost {
    let witnesses = [
        (TargetSource::SessionRecord, &evidence.session_record),
        (TargetSource::InflightLocator, &evidence.inflight_locator),
        (TargetSource::HostMarker, &evidence.host_marker),
    ];
    let mut known = Vec::new();
    for (source, witness) in witnesses {
        match witness {
            HostWitness::Absent => {}
            HostWitness::Known { kind, target } => {
                let target = target.as_deref().filter(|name| !name.trim().is_empty());
                known.push((*kind, source, target));
            }
            HostWitness::Unrecognized(detail) | HostWitness::ReadFailed(detail) => {
                let detail = detail.clone();
                return TargetHost::Unknown(UnknownHost::Unreadable { source, detail });
            }
        }
    }
    if evidence.runtime_kind_unrecognized {
        return TargetHost::Unknown(UnknownHost::Unreadable {
            source: TargetSource::Legacy(HostKindSource::DurableRuntimeKind),
            detail: "unrecognized runtime kind".to_string(),
        });
    }
    let session_name = evidence
        .session_name
        .as_deref()
        .filter(|name| !name.trim().is_empty());
    let Some(&(kind, source, _)) = known.first() else {
        return legacy_target_host(evidence, session_name);
    };
    let first = (kind, source);
    let named = known.iter().find_map(|(_, _, target)| *target);
    let differs = known.iter().find(|(other, _, target)| {
        *other != kind || target.is_some_and(|target| Some(target) != named)
    });
    if let Some(&(other, second, _)) = differs {
        return TargetHost::Conflict {
            first,
            second: (other, second),
        };
    }
    if let Some(vote) = host_votes(evidence).find(|(other, _)| *other != kind) {
        return TargetHost::Conflict {
            first,
            second: (vote.0, TargetSource::Legacy(vote.1)),
        };
    }
    let name = match (kind, named) {
        (_, Some(name)) => name,
        (HostKind::Tmux | HostKind::Process, None) => match session_name {
            Some(name) => name,
            None => return TargetHost::Unknown(UnknownHost::MissingTarget(kind)),
        },
        (HostKind::Herdr, None) => return TargetHost::Unknown(UnknownHost::MissingTarget(kind)),
    };
    TargetHost::Known {
        kind,
        source,
        name: name.to_string(),
    }
}

/// Legacy votes that name a host. ClaudeTui/CodexTui run on any host, so they
/// never outvote an explicit witness.
fn host_votes(
    evidence: &SessionTargetEvidence,
) -> impl Iterator<Item = (HostKind, HostKindSource)> + '_ {
    let host_bound = |kind: &RuntimeHandoffKind| {
        !matches!(
            kind,
            RuntimeHandoffKind::ClaudeTui | RuntimeHandoffKind::CodexTui
        )
    };
    let durable = [
        evidence.durable_runtime_kind,
        evidence.inflight_runtime_kind,
    ]
    .into_iter()
    .flatten()
    .filter(host_bound)
    .map(|kind| (kind_hint(kind), HostKindSource::DurableRuntimeKind));
    let marker = evidence
        .runtime_kind_marker
        .filter(host_bound)
        .map(|kind| (kind_hint(kind), HostKindSource::RuntimeKindMarker));
    let registry = evidence
        .process_registry_hit
        .then_some((HostKind::Process, HostKindSource::ProcessRegistry));
    durable.chain(marker).chain(registry)
}

fn legacy_target_host(evidence: &SessionTargetEvidence, session_name: Option<&str>) -> TargetHost {
    let durable = match (
        evidence.durable_runtime_kind,
        evidence.inflight_runtime_kind,
    ) {
        (Some(caller), Some(row)) if kind_hint(caller) != kind_hint(row) => {
            let source = TargetSource::Legacy(HostKindSource::DurableRuntimeKind);
            return TargetHost::Conflict {
                first: (kind_hint(caller), source),
                second: (kind_hint(row), source),
            };
        }
        (caller, row) => caller.or(row),
    };
    let legacy = HostEvidence {
        session_name,
        durable_runtime_kind: durable,
        runtime_kind_marker: evidence.runtime_kind_marker,
        process_registry_hit: evidence.process_registry_hit,
    };
    match (resolve_host_kind(legacy), session_name) {
        (HostKindResolution::Known { kind, source }, Some(name)) => TargetHost::Known {
            kind,
            source: TargetSource::Legacy(source),
            name: name.to_string(),
        },
        (HostKindResolution::Conflict { first, second }, _) => TargetHost::Conflict {
            first: (first.0, TargetSource::Legacy(first.1)),
            second: (second.0, TargetSource::Legacy(second.1)),
        },
        _ => TargetHost::Unknown(UnknownHost::NoHostEvidence),
    }
}

static TMUX_HOST: TmuxHost = TmuxHost;
static PROCESS_HOST: ProcessHost = ProcessHost;
static UNCONFIGURED_HERDR_HOST: UnconfiguredHerdrHost = UnconfiguredHerdrHost;

/// Herdr has no static endpoint, so it gets the fail-closed host, never a default socket.
pub(crate) fn host_for(kind: HostKind) -> &'static dyn InteractiveSessionHost {
    match kind {
        HostKind::Tmux => &TMUX_HOST,
        HostKind::Process => &PROCESS_HOST,
        HostKind::Herdr => &UNCONFIGURED_HERDR_HOST,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use HostKindSource::{DurableRuntimeKind, ProcessRegistry, RuntimeKindMarker};
    use RuntimeHandoffKind as R;

    fn named() -> HostEvidence<'static> {
        HostEvidence {
            session_name: Some("AgentDesk-claude-test"),
            ..HostEvidence::default()
        }
    }

    fn known(kind: HostKind, source: HostKindSource) -> HostKindResolution {
        HostKindResolution::Known { kind, source }
    }

    #[test]
    fn missing_or_blank_session_name_is_unknown_even_with_evidence() {
        for session_name in [None, Some(""), Some("   ")] {
            let evidence = HostEvidence {
                session_name,
                durable_runtime_kind: Some(R::ClaudeTui),
                runtime_kind_marker: Some(R::ClaudeTui),
                process_registry_hit: true,
            };
            assert_eq!(resolve_host_kind(evidence), HostKindResolution::Unknown);
        }
    }

    #[test]
    fn no_evidence_is_unknown_not_a_tmux_default() {
        assert_eq!(resolve_host_kind(named()), HostKindResolution::Unknown);
    }

    #[test]
    fn single_vote_is_known_with_its_source() {
        let durable = HostEvidence {
            durable_runtime_kind: Some(R::ClaudeTui),
            ..named()
        };
        assert_eq!(
            resolve_host_kind(durable),
            known(HostKind::Tmux, DurableRuntimeKind)
        );
        let marker = HostEvidence {
            runtime_kind_marker: Some(R::ProcessBackend),
            ..named()
        };
        assert_eq!(
            resolve_host_kind(marker),
            known(HostKind::Process, RuntimeKindMarker)
        );
        let registry = HostEvidence {
            process_registry_hit: true,
            ..named()
        };
        assert_eq!(
            resolve_host_kind(registry),
            known(HostKind::Process, ProcessRegistry)
        );
    }

    #[test]
    fn agreeing_votes_report_the_first_source() {
        let evidence = HostEvidence {
            durable_runtime_kind: Some(R::ClaudeEAdapter),
            runtime_kind_marker: Some(R::ProcessBackend),
            process_registry_hit: true,
            ..named()
        };
        assert_eq!(
            resolve_host_kind(evidence),
            known(HostKind::Process, DurableRuntimeKind)
        );
    }

    #[test]
    fn durable_tmux_with_registry_hit_is_a_conflict() {
        let evidence = HostEvidence {
            durable_runtime_kind: Some(R::ClaudeTui),
            process_registry_hit: true,
            ..named()
        };
        assert_eq!(
            resolve_host_kind(evidence),
            HostKindResolution::Conflict {
                first: (HostKind::Tmux, DurableRuntimeKind),
                second: (HostKind::Process, ProcessRegistry),
            }
        );
    }

    #[test]
    fn conflict_pairs_the_first_vote_with_the_first_disagreeing_vote() {
        // Process / Tmux / Process: the second Process vote agrees with the first.
        let evidence = HostEvidence {
            durable_runtime_kind: Some(R::ProcessBackend),
            runtime_kind_marker: Some(R::ClaudeTui),
            process_registry_hit: true,
            ..named()
        };
        assert_eq!(
            resolve_host_kind(evidence),
            HostKindResolution::Conflict {
                first: (HostKind::Process, DurableRuntimeKind),
                second: (HostKind::Tmux, RuntimeKindMarker),
            }
        );
    }

    #[test]
    fn kind_hint_maps_every_runtime_kind() {
        for (runtime, host) in [
            (R::ProcessBackend, HostKind::Process),
            (R::ClaudeEAdapter, HostKind::Process),
            (R::LegacyTmuxWrapper, HostKind::Tmux),
            (R::ClaudeTui, HostKind::Tmux),
            (R::CodexTui, HostKind::Tmux),
        ] {
            assert_eq!(kind_hint(runtime), host, "{runtime:?}");
        }
    }

    #[test]
    fn host_for_returns_the_host_of_that_kind() {
        for kind in [HostKind::Tmux, HostKind::Process] {
            assert_eq!(host_for(kind).kind(), kind);
        }
    }

    #[test]
    fn herdr_host_for_without_endpoint_fails_explicitly() {
        let host = host_for(HostKind::Herdr);
        let session = crate::services::session_host::HostSessionRef::herdr_pane("w1-1");
        assert_eq!(
            host.send_text(session, "x"),
            Err(crate::services::session_host::HostError::Unsupported(
                HostKind::Herdr,
                "endpoint_missing"
            )),
            "host_for(Herdr) must not pick a default socket"
        );
        assert_eq!(host.kind(), HostKind::Herdr);
        assert_eq!(
            host.presence(session),
            crate::services::session_host::HostPresence::ProbeFailed
        );
    }

    #[test]
    fn herdr_tui_runtime_kinds_without_host_evidence_stay_non_herdr() {
        // No runtime kind names a host of Herdr; persisted values are unchanged.
        for runtime in [R::ClaudeTui, R::CodexTui] {
            assert_eq!(kind_hint(runtime), HostKind::Tmux);
        }
        let herdr_named = HostEvidence {
            session_name: Some("w1-1"),
            ..HostEvidence::default()
        };
        assert_eq!(
            resolve_host_kind(herdr_named),
            HostKindResolution::Unknown,
            "Unknown must not become a tmux default"
        );
    }

    struct FakeSource {
        evidence: SessionTargetEvidence,
        inputs: std::cell::RefCell<Vec<SessionTargetInput>>,
    }

    impl SessionTargetEvidenceSource for FakeSource {
        fn read_evidence(&self, input: &SessionTargetInput) -> SessionTargetEvidence {
            self.inputs.borrow_mut().push(input.clone());
            self.evidence.clone()
        }
    }

    fn resolve(
        input: SessionTargetInput,
        evidence: SessionTargetEvidence,
    ) -> ResolvedSessionTarget {
        let source = FakeSource {
            evidence,
            inputs: Default::default(),
        };
        let resolved = resolve_session_target(input.clone(), &source);
        assert_eq!(
            source.inputs.into_inner(),
            vec![input],
            "one read, whole input"
        );
        resolved
    }

    fn key(key: &str) -> SessionTargetInput {
        SessionTargetInput::SessionKey(key.to_string())
    }

    fn absent() -> SessionTargetEvidence {
        SessionTargetEvidence {
            session_record: HostWitness::Absent,
            inflight_locator: HostWitness::Absent,
            host_marker: HostWitness::Absent,
            ..SessionTargetEvidence::unread()
        }
    }

    fn witness(kind: HostKind, target: Option<&str>) -> HostWitness {
        HostWitness::Known {
            kind,
            target: target.map(str::to_string),
        }
    }

    fn target(kind: HostKind, source: TargetSource, name: &str) -> TargetHost {
        TargetHost::Known {
            kind,
            source,
            name: name.to_string(),
        }
    }

    const TUI_NAME: &str = "AgentDesk-claude-adk:cc";

    #[test]
    fn full_session_key_is_kept_and_no_name_comes_from_its_suffix() {
        for full in [
            "claude/hash123/mac-mini:AgentDesk-claude-adk:cc",
            "claude:1473922824350601297:agentdesk-claude-channel-1473922824350601297",
            "claude/hash123/mac-mini:herdr:w1-1",
            "zellij/hash123/mac-mini:session:7",
        ] {
            let unnamed = SessionTargetEvidence {
                session_key: Some(full.to_string()),
                durable_runtime_kind: Some(R::ClaudeTui),
                ..absent()
            };
            let resolved = resolve(key(full), unnamed.clone());
            assert_eq!(resolved.input, key(full));
            assert_eq!(resolved.session_key.as_deref(), Some(full));
            assert_eq!(
                resolved.host,
                TargetHost::Unknown(UnknownHost::NoHostEvidence),
                "{full}: a key suffix must never become the tmux name"
            );
            assert_eq!(resolved.legacy_ref(), None, "{full}");

            let named = SessionTargetEvidence {
                session_name: Some(TUI_NAME.to_string()),
                ..unnamed
            };
            let resolved = resolve(key(full), named);
            assert_eq!(
                resolved.host,
                target(
                    HostKind::Tmux,
                    TargetSource::Legacy(DurableRuntimeKind),
                    TUI_NAME
                ),
                "{full}: the recorded name is kept whole, colons included"
            );
            assert_eq!(resolved.legacy_ref(), Some(HostSessionRef::tmux(TUI_NAME)));
        }
        let canonical = SessionTargetInput::Canonical {
            provider: "claude".to_string(),
            token_hash: "hash123".to_string(),
            channel_id: 1473922824350601297,
        };
        assert_eq!(resolve(canonical.clone(), absent()).input, canonical);
    }

    #[test]
    fn each_known_host_comes_from_its_witness() {
        let named = |evidence: SessionTargetEvidence| SessionTargetEvidence {
            session_name: Some(TUI_NAME.to_string()),
            ..evidence
        };
        let cases = [
            (
                SessionTargetEvidence {
                    inflight_locator: witness(HostKind::Tmux, Some("AgentDesk-codex-x")),
                    durable_runtime_kind: Some(R::CodexTui),
                    ..absent()
                },
                target(
                    HostKind::Tmux,
                    TargetSource::InflightLocator,
                    "AgentDesk-codex-x",
                ),
            ),
            (
                SessionTargetEvidence {
                    session_record: witness(HostKind::Process, Some("proc-1")),
                    durable_runtime_kind: Some(R::ProcessBackend),
                    process_registry_hit: true,
                    ..absent()
                },
                target(HostKind::Process, TargetSource::SessionRecord, "proc-1"),
            ),
            (
                named(SessionTargetEvidence {
                    host_marker: witness(HostKind::Tmux, None),
                    durable_runtime_kind: Some(R::ClaudeTui),
                    ..absent()
                }),
                target(HostKind::Tmux, TargetSource::HostMarker, TUI_NAME),
            ),
            (
                named(SessionTargetEvidence {
                    session_record: witness(HostKind::Herdr, Some("w1-1")),
                    inflight_locator: witness(HostKind::Herdr, Some("w1-1")),
                    host_marker: witness(HostKind::Herdr, None),
                    durable_runtime_kind: Some(R::CodexTui),
                    runtime_kind_marker: Some(R::ClaudeTui),
                    ..absent()
                }),
                target(HostKind::Herdr, TargetSource::SessionRecord, "w1-1"),
            ),
        ];
        for (evidence, expected) in cases {
            let resolved = resolve(key("claude/h/mac-mini:x"), evidence);
            assert_eq!(resolved.host, expected);
        }
        for pane in [None, Some(" ")] {
            let herdr_without_pane = named(SessionTargetEvidence {
                host_marker: witness(HostKind::Herdr, pane),
                ..absent()
            });
            assert_eq!(
                resolve(key("claude/h/mac-mini:x"), herdr_without_pane).host,
                TargetHost::Unknown(UnknownHost::MissingTarget(HostKind::Herdr)),
                "{pane:?}: a Herdr target never borrows the tmux session name"
            );
        }
    }

    #[test]
    fn herdr_record_outlives_marker_and_inflight_loss() {
        let lost_marker = SessionTargetEvidence {
            session_name: Some(TUI_NAME.to_string()),
            session_record: witness(HostKind::Herdr, Some("w1-1")),
            durable_runtime_kind: Some(R::ClaudeTui),
            runtime_kind_marker: Some(R::ClaudeTui),
            ..absent()
        };
        let resolved = resolve(key("claude/h/mac-mini:x"), lost_marker.clone());
        assert_eq!(
            resolved.host,
            target(HostKind::Herdr, TargetSource::SessionRecord, "w1-1"),
            "TUI runtime kinds must not outvote the Herdr record"
        );
        assert_eq!(resolved.legacy_ref(), None);

        let legacy_row = SessionTargetEvidence {
            session_record: HostWitness::Absent,
            ..lost_marker
        };
        assert_eq!(
            resolve(key("claude/h/mac-mini:x"), legacy_row).host,
            target(
                HostKind::Tmux,
                TargetSource::Legacy(DurableRuntimeKind),
                TUI_NAME
            ),
            "a row with no host witness keeps its legacy tmux reading"
        );
    }

    #[test]
    fn unreadable_or_future_witness_is_unknown_despite_legacy_tmux_votes() {
        let legacy_tmux = SessionTargetEvidence {
            session_name: Some(TUI_NAME.to_string()),
            durable_runtime_kind: Some(R::ClaudeTui),
            runtime_kind_marker: Some(R::LegacyTmuxWrapper),
            ..absent()
        };
        let bad = [
            HostWitness::Unrecognized("zellij".to_string()),
            HostWitness::Unrecognized(r#"{"host_kind":"herdr"}"#.to_string()),
            HostWitness::ReadFailed("permission denied".to_string()),
        ];
        let sources = [
            TargetSource::SessionRecord,
            TargetSource::InflightLocator,
            TargetSource::HostMarker,
        ];
        for witness in bad {
            for source in sources {
                let mut evidence = legacy_tmux.clone();
                *match source {
                    TargetSource::SessionRecord => &mut evidence.session_record,
                    TargetSource::InflightLocator => &mut evidence.inflight_locator,
                    _ => &mut evidence.host_marker,
                } = witness.clone();
                let resolved = resolve(key("claude/h/mac-mini:x"), evidence);
                assert!(
                    matches!(
                        &resolved.host,
                        TargetHost::Unknown(UnknownHost::Unreadable { source: s, .. }) if *s == source
                    ),
                    "{source:?} {witness:?}: must stay Unknown, never tmux; got {:?}",
                    resolved.host
                );
                assert_eq!(resolved.legacy_ref(), None);
            }
        }
        let unread = SessionTargetEvidence {
            session_name: legacy_tmux.session_name.clone(),
            durable_runtime_kind: legacy_tmux.durable_runtime_kind,
            ..SessionTargetEvidence::unread()
        };
        let future_kind = SessionTargetEvidence {
            runtime_kind_unrecognized: true,
            ..legacy_tmux
        };
        for evidence in [unread, future_kind] {
            let resolved = resolve(key("claude/h/mac-mini:x"), evidence);
            assert!(
                matches!(
                    resolved.host,
                    TargetHost::Unknown(UnknownHost::Unreadable { .. })
                ),
                "an unread witness or unknown runtime kind must stay Unknown; got {:?}",
                resolved.host
            );
        }
    }

    #[test]
    fn disagreeing_witnesses_are_a_conflict() {
        let conflict = |first, second| TargetHost::Conflict { first, second };
        let cases = [
            (
                SessionTargetEvidence {
                    session_record: witness(HostKind::Herdr, Some("w1-1")),
                    inflight_locator: witness(HostKind::Tmux, Some(TUI_NAME)),
                    ..absent()
                },
                conflict(
                    (HostKind::Herdr, TargetSource::SessionRecord),
                    (HostKind::Tmux, TargetSource::InflightLocator),
                ),
            ),
            (
                SessionTargetEvidence {
                    session_record: witness(HostKind::Herdr, Some("w1-1")),
                    inflight_locator: witness(HostKind::Herdr, Some("w1-2")),
                    ..absent()
                },
                conflict(
                    (HostKind::Herdr, TargetSource::SessionRecord),
                    (HostKind::Herdr, TargetSource::InflightLocator),
                ),
            ),
            (
                SessionTargetEvidence {
                    inflight_locator: witness(HostKind::Herdr, Some("w1-1")),
                    process_registry_hit: true,
                    ..absent()
                },
                conflict(
                    (HostKind::Herdr, TargetSource::InflightLocator),
                    (HostKind::Process, TargetSource::Legacy(ProcessRegistry)),
                ),
            ),
            (
                SessionTargetEvidence {
                    host_marker: witness(HostKind::Herdr, None),
                    durable_runtime_kind: Some(R::LegacyTmuxWrapper),
                    ..absent()
                },
                conflict(
                    (HostKind::Herdr, TargetSource::HostMarker),
                    (HostKind::Tmux, TargetSource::Legacy(DurableRuntimeKind)),
                ),
            ),
            (
                SessionTargetEvidence {
                    session_name: Some(TUI_NAME.to_string()),
                    durable_runtime_kind: Some(R::ClaudeTui),
                    process_registry_hit: true,
                    ..absent()
                },
                conflict(
                    (HostKind::Tmux, TargetSource::Legacy(DurableRuntimeKind)),
                    (HostKind::Process, TargetSource::Legacy(ProcessRegistry)),
                ),
            ),
        ];
        for (evidence, expected) in cases {
            let resolved = resolve(key("claude/h/mac-mini:x"), evidence);
            assert_eq!(resolved.host, expected, "disagreement must stay a Conflict");
            assert_eq!(resolved.legacy_ref(), None);
        }
    }

    #[test]
    fn raw_name_reaches_tmux_only_with_legacy_tmux_evidence() {
        let raw = SessionTargetInput::RawName(TUI_NAME.to_string());
        let bare = SessionTargetEvidence {
            session_name: Some(TUI_NAME.to_string()),
            ..absent()
        };
        let resolved = resolve(raw.clone(), bare.clone());
        assert_eq!(
            resolved.host,
            TargetHost::Unknown(UnknownHost::NoHostEvidence),
            "a raw name without legacy evidence must not become a tmux ref"
        );
        assert_eq!(resolved.legacy_ref(), None);

        let marked = SessionTargetEvidence {
            runtime_kind_marker: Some(R::ClaudeTui),
            ..bare
        };
        let resolved = resolve(raw, marked);
        assert_eq!(
            resolved.host,
            target(
                HostKind::Tmux,
                TargetSource::Legacy(RuntimeKindMarker),
                TUI_NAME
            )
        );
        assert_eq!(resolved.legacy_ref(), Some(HostSessionRef::tmux(TUI_NAME)));
    }

    #[test]
    fn host_marker_on_disk_replaces_the_tmux_vote_and_conflicts_with_another_host() {
        use crate::services::tmux_common::{host_marker, session_temp_path};
        let _root = crate::config::TestRuntimeRootGuard::new();
        let name = "AgentDesk-claude-marker-resolve";
        let session_key = format!("claude/h/mac-mini:{name}");
        let read = |evidence: SessionTargetEvidence| {
            let evidence = SessionTargetEvidence {
                session_key: Some(session_key.clone()),
                ..evidence
            }
            .with_host_marker();
            resolve(key(&session_key), evidence).host
        };
        let tui_row = SessionTargetEvidence {
            session_name: Some(name.to_string()),
            inflight_runtime_kind: Some(R::ClaudeTui),
            runtime_kind_marker: Some(R::ClaudeTui),
            ..absent()
        };
        assert_eq!(
            read(tui_row.clone()),
            target(
                HostKind::Tmux,
                TargetSource::Legacy(DurableRuntimeKind),
                name
            ),
            "no marker keeps the legacy reading"
        );

        host_marker::record_tmux_host_marker(name);
        assert_eq!(
            read(tui_row.clone()),
            target(HostKind::Tmux, TargetSource::HostMarker, name)
        );
        let other_host = [
            SessionTargetEvidence {
                inflight_locator: witness(HostKind::Tmux, Some("AgentDesk-claude-other")),
                ..tui_row.clone()
            },
            SessionTargetEvidence {
                inflight_locator: witness(HostKind::Process, None),
                ..tui_row.clone()
            },
            SessionTargetEvidence {
                process_registry_hit: true,
                ..tui_row.clone()
            },
        ];
        for evidence in other_host {
            assert!(
                matches!(read(evidence.clone()), TargetHost::Conflict { .. }),
                "{evidence:?}"
            );
        }

        std::fs::write(session_temp_path(name, "host_kind"), "herdr").unwrap();
        let herdr = read(tui_row.clone());
        assert_eq!(
            herdr,
            TargetHost::Unknown(UnknownHost::MissingTarget(HostKind::Herdr)),
            "a TUI runtime kind must not turn a non-tmux marker into tmux"
        );
        for (written, label) in [("zellij", "a future host"), ("", "a truncated marker")] {
            std::fs::write(session_temp_path(name, "host_kind"), written).unwrap();
            assert!(
                matches!(
                    read(tui_row.clone()),
                    TargetHost::Unknown(UnknownHost::Unreadable {
                        source: TargetSource::HostMarker,
                        ..
                    })
                ),
                "{label} is Unknown, never the legacy tmux reading"
            );
        }
        let keyless = SessionTargetEvidence::unread().with_host_marker();
        assert!(
            matches!(keyless.host_marker, HostWitness::ReadFailed(_)),
            "without a key the marker stays unread"
        );
    }
}
