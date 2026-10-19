//! Read-only Unbound evidence; no result here grants a start capability.
use std::collections::BTreeMap;

use crate::config::{
    Config,
    session_hosts::{BootSessionHosts, ChannelEndpoint},
};
use crate::db::dispatched_sessions::hosted_execution::{
    HostedLookup, HostedObservation, HostedOwner, HostedRecord, HostedState,
};
use crate::services::discord::{
    adk_session,
    inflight::{InflightTurnState, marker_witness, with_inflight_row},
    settings::RegisteredChannelBinding,
};
use crate::services::platform::tmux::session_inventory::Inventory;
use crate::services::provider::ProviderKind;
use crate::services::provider_hosting::channel_effective_tui_request;
use crate::services::session_host::{
    HostKind, HostWitness, SessionTargetEvidence, SessionTargetEvidenceSource, SessionTargetInput,
    TargetHost, resolve_session_target, session_record_witness,
};
use crate::services::tmux_common::host_marker::HostKindMarker;
use crate::services::tui_o::{
    shadow::ShadowProvider,
    writer::binding::{BindingEvent, BindingEvents, BindingRecord},
};

#[derive(Debug, Clone, PartialEq, Eq)]
enum Origin {
    Core,
    Fallback,
    Binding(u64),
    Inflight,
}
#[derive(Debug, Clone, PartialEq, Eq)]
struct Candidate {
    name: String,
    origin: Origin,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Candidates {
    channel: u64,
    provider: ProviderKind,
    current: Option<String>,
    names: Vec<Candidate>,
    conflict: bool,
    inflight: bool,
}

pub(super) fn observe_candidates(
    channel: u64,
    provider: &ProviderKind,
    core: Option<&str>,
    fallback: &[RegisteredChannelBinding],
    log: &dyn BindingEvents,
) -> Result<Candidates, String> {
    let inflight =
        crate::services::discord::inflight::load_inflight_state_read_only_result(provider, channel);
    unbound_candidates(
        channel,
        provider,
        core,
        fallback,
        log.binding_events_since(channel, 0),
        &inflight,
    )
}

fn unbound_candidates(
    channel: u64,
    provider: &ProviderKind,
    core: Option<&str>,
    fallback: &[RegisteredChannelBinding],
    events: Result<Vec<BindingEvent>, String>,
    inflight: &Result<Option<InflightTurnState>, String>,
) -> Result<Candidates, String> {
    #[cfg(test)]
    dormancy_tests::reached(channel);
    let events = events?;
    let row = inflight.as_ref().map_err(Clone::clone)?;
    let shadow = match provider {
        ProviderKind::Claude => ShadowProvider::Claude,
        ProviderKind::Codex => ShadowProvider::Codex,
        _ => return Err("provider unsupported".into()),
    };
    let mut names = Vec::new();
    if let Some(basis) = core {
        if basis.trim().is_empty() {
            return Err("blank core basis".into());
        }
        names.push(Candidate {
            name: provider.build_tmux_session_name(basis),
            origin: Origin::Core,
        });
    }
    for registered in fallback
        .iter()
        .filter(|r| r.channel_id == channel && r.owner_provider == *provider)
    {
        if let Some(basis) = &registered.fallback_name {
            if basis.trim().is_empty() {
                return Err("blank fallback basis".into());
            }
            names.push(Candidate {
                name: provider.build_tmux_session_name(basis),
                origin: Origin::Fallback,
            });
        }
    }
    let current = names.first().map(|c| c.name.clone());
    let conflict = names.iter().any(|c| Some(&c.name) != current.as_ref());
    let mut previous = 0;
    for event in &events {
        if event.channel_id != channel || event.provider != shadow || event.seq <= previous {
            return Err("foreign or unordered binding".into());
        }
        previous = event.seq;
    }
    if let Some(event) = events
        .iter()
        .rev()
        .find(|e| !matches!(e.record, BindingRecord::Rejected { .. }))
    {
        names.push(Candidate {
            name: event.tmux_session.clone(),
            origin: Origin::Binding(event.seq),
        });
    }
    if let Some(row) = row {
        if row.channel_id != channel
            || row.provider != provider.as_str()
            || row.runtime_kind_unknown_on_disk
        {
            return Err("foreign or unreadable inflight".into());
        }
        if let Some(name) = &row.tmux_session_name {
            names.push(Candidate {
                name: name.clone(),
                origin: Origin::Inflight,
            });
        }
    }
    if names.iter().any(|c| c.name.trim().is_empty()) {
        return Err("blank candidate".into());
    }
    Ok(Candidates {
        channel,
        provider: provider.clone(),
        current,
        names,
        conflict,
        inflight: row.is_some(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Scope {
    LocalInventory,
    LocalNameless,
    HerdrFirstCreate,
    HerdrRetired,
    Hold(&'static str),
}
struct HostScope {
    channel: u64,
    provider: ProviderKind,
    name: String,
    result: Scope,
}
struct HostReads<'a> {
    channel: u64,
    provider: &'a ProviderKind,
    token_hash: &'a str,
    name: &'a str,
    config: &'a Config,
    expected_owner: &'a HostedOwner,
    exact_key: &'a str,
    exact: &'a HostedLookup,
    canonical: &'a HostedLookup,
    evidence: Result<SessionTargetEvidence, String>,
    marker: HostKindMarker,
    inflight: &'a Result<Option<InflightTurnState>, String>,
}
struct Evidence(SessionTargetEvidence);
impl SessionTargetEvidenceSource for Evidence {
    fn read_evidence(&self, _: &SessionTargetInput) -> SessionTargetEvidence {
        self.0.clone()
    }
}
fn host_scope(reads: HostReads<'_>) -> HostScope {
    let (channel, provider, name) = (reads.channel, reads.provider.clone(), reads.name.to_owned());
    HostScope {
        channel,
        provider,
        name,
        result: host_scope_result(reads),
    }
}
fn host_scope_result(reads: HostReads<'_>) -> Scope {
    #[cfg(test)]
    dormancy_tests::reached(reads.channel);
    let hold = Scope::Hold;
    let (Ok(mut evidence), Ok(row)) = (reads.evidence, reads.inflight.as_ref()) else {
        return hold("probe_failed");
    };
    let witnesses = [
        &evidence.session_record,
        &evidence.inflight_locator,
        &evidence.host_marker,
    ];
    if witnesses
        .iter()
        .any(|w| matches!(w, HostWitness::ReadFailed(_)))
        || matches!(reads.exact, HostedLookup::Unknown(_))
        || matches!(reads.canonical, HostedLookup::Unknown(_))
    {
        return hold("probe_failed");
    }
    let marker = observed_marker(reads.marker);
    if matches!(marker, HostWitness::ReadFailed(_)) {
        return hold("probe_failed");
    }
    if evidence.host_marker != marker {
        return hold("marker_witness_mismatch");
    }
    let key =
        adk_session::build_namespaced_session_key(reads.token_hash, reads.provider, reads.name);
    if reads.exact_key != key
        || evidence.session_key.as_deref().is_some_and(|k| k != key)
        || evidence
            .session_name
            .as_deref()
            .is_some_and(|n| n != reads.name)
    {
        return hold("key_witness_mismatch");
    }
    if reads.exact != reads.canonical {
        return hold("exact_canonical_mismatch");
    }
    evidence.session_key = Some(key.clone());
    evidence.session_name = Some(reads.name.into());
    if evidence.session_record != observed_row(reads.exact) {
        return hold("record_witness_mismatch");
    }
    if let Some(row) = row {
        evidence = with_inflight_row(evidence, row);
    }
    if evidence.name_conflict.is_some() {
        return hold("inflight_name_conflict");
    }
    if evidence.runtime_kind_unrecognized
        || [
            &evidence.session_record,
            &evidence.inflight_locator,
            &evidence.host_marker,
        ]
        .iter()
        .any(|w| {
            matches!(
                w,
                HostWitness::Unrecognized(_) | HostWitness::RowConflict(_)
            )
        })
    {
        return hold("witness_unrecognized");
    }
    let bindings: Vec<_> = reads
        .config
        .agents
        .iter()
        .flat_map(|a| a.channels.iter())
        .filter(|(_, b)| {
            b.channel_id().and_then(|id| id.parse::<u64>().ok()) == Some(reads.channel)
        })
        .collect();
    if bindings.is_empty()
        || bindings.iter().any(|(provider, binding)| {
            let provider = binding
                .provider()
                .unwrap_or_else(|| (*provider).into())
                .trim()
                .to_ascii_lowercase();
            provider != reads.provider.as_str()
                || !channel_effective_tui_request(reads.config, binding, &provider)
        })
    {
        return hold("host_unsupported");
    }
    let Ok(boot) = BootSessionHosts::from_config(reads.config) else {
        return hold("probe_failed");
    };
    let quiet = row.is_none()
        && evidence.inflight_locator == HostWitness::Absent
        && evidence.durable_runtime_kind.is_none()
        && evidence.inflight_runtime_kind.is_none()
        && evidence.runtime_kind_marker.is_none()
        && !evidence.process_registry_hit;
    let clean = quiet && evidence.host_marker == HostWitness::Absent;
    let retired_marker = matches!(
        evidence.host_marker,
        HostWitness::Absent
            | HostWitness::Known {
                kind: HostKind::Herdr,
                target: None
            }
    );
    if let Some(endpoint) = boot.herdr_endpoint(reads.channel) {
        let owner = reads.expected_owner;
        let (provider, token_hash) = (reads.provider, reads.token_hash);
        if !local_owner(
            owner,
            provider,
            reads.channel,
            token_hash,
            reads.name,
            boot.local_node(),
        ) {
            return hold("herdr_owner_mismatch");
        }
        return match reads.exact {
            HostedLookup::Missing if clean => Scope::HerdrFirstCreate,
            HostedLookup::Found(found)
                if quiet
                    && retired_marker
                    && herdr_record_at(found, HostedState::Retired, owner, endpoint) =>
            {
                Scope::HerdrRetired
            }
            _ => hold("hosted_active_or_unknown"),
        };
    }
    if row.is_some() {
        return hold("inflight_present");
    }
    if matches!(reads.exact, HostedLookup::Missing) && clean {
        return Scope::LocalInventory;
    }
    let resolved = resolve_session_target(SessionTargetInput::SessionKey(key), &Evidence(evidence));
    match resolved.host {
        TargetHost::Known {
            kind: HostKind::Tmux,
            name,
            ..
        } if name == reads.name => Scope::LocalInventory,
        _ => hold("host_unresolved"),
    }
}

/// Whether `owner` names this channel's session `name` on this node, under a runtime root.
fn local_owner(
    owner: &HostedOwner,
    provider: &ProviderKind,
    channel: u64,
    token_hash: &str,
    name: &str,
    node: Option<&str>,
) -> bool {
    owner.provider == provider.as_str()
        && owner.channel_id == channel.to_string()
        && owner.discord_token_hash == token_hash
        && owner.logical_key == name
        && Some(owner.owner_node.as_str()) == node
        && !owner.runtime_root.trim().is_empty()
}

/// Whether `found` is a self-consistent record in `state`, owned by `owner` at `endpoint`.
fn herdr_record_at(
    found: &HostedObservation,
    state: HostedState,
    owner: &HostedOwner,
    endpoint: &ChannelEndpoint,
) -> bool {
    let HostedRecord::Known(record) = &found.record else {
        return false;
    };
    HostedRecord::decode(serde_json::to_value(record).ok().as_ref()) == found.record
        && record.state == state
        && record.owner == *owner
        && record.location.as_ref().is_some_and(|at| {
            at.host == "herdr"
                && at.execution_node == endpoint.execution_node
                && at.endpoint_config_key == endpoint.key
                && at.socket_addr == endpoint.socket_path.to_string_lossy()
                && at.named_session == endpoint.herdr_session
        })
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Absence {
    LocalInventory,
    HerdrFirstCreate,
    HerdrRetired,
}
#[derive(Debug, Clone, PartialEq, Eq)]
enum Verdict {
    Absent(Absence),
    Hold(&'static str),
}
fn decide_unbound(
    candidates: Result<Candidates, String>,
    scope: HostScope,
    inventory: &Inventory,
    owners: &BTreeMap<String, Option<u64>>,
) -> Verdict {
    let Ok(candidates) = candidates else {
        return Verdict::Hold("probe_failed");
    };
    #[cfg(test)]
    dormancy_tests::reached(candidates.channel);
    if candidates.conflict {
        return Verdict::Hold("candidate_conflict");
    }
    if candidates.inflight {
        return Verdict::Hold("candidate_inflight");
    }
    if scope.channel != candidates.channel
        || scope.provider != candidates.provider
        || candidates
            .current
            .as_ref()
            .is_some_and(|name| name != &scope.name)
    {
        return Verdict::Hold("scope_mismatch");
    }
    if candidates.current.is_none()
        && !matches!(
            scope.result,
            Scope::LocalInventory | Scope::LocalNameless | Scope::Hold(_)
        )
    {
        return Verdict::Hold("hosted_candidate_unresolved");
    }
    if matches!(scope.result, Scope::LocalNameless) && candidates.current.is_some() {
        return Verdict::Hold("nameless_with_current");
    }
    let hosted = match scope.result {
        Scope::Hold(reason) => return Verdict::Hold(reason),
        Scope::HerdrFirstCreate => Some(Absence::HerdrFirstCreate),
        Scope::HerdrRetired => Some(Absence::HerdrRetired),
        Scope::LocalInventory | Scope::LocalNameless => None,
    };
    let names = match inventory {
        Inventory::Sessions(names) => names.as_slice(),
        Inventory::Empty(_) => &[],
        Inventory::ProbeFailed(_) => {
            let trace = candidates.names.iter().any(|c| {
                matches!(c.origin, Origin::Binding(_) | Origin::Inflight)
                    || owners.get(&c.name).is_some_and(Option::is_some)
            });
            return match (hosted, trace) {
                (Some(absence), false) => Verdict::Absent(absence),
                _ => Verdict::Hold("probe_failed"),
            };
        }
    };
    if names.iter().any(|name| {
        candidates.names.iter().any(|c| c.name == *name)
            || owners.get(name) == Some(&Some(candidates.channel))
    }) {
        return Verdict::Hold("watcher_unbound");
    }
    // Only a name in AgentDesk's tmux namespace can be this channel's session under a name its
    // own evidence lacks; a session it names or owns already held above.
    let namespace = format!("{}-", crate::services::provider::TMUX_SESSION_PREFIX);
    if names.iter().any(|name| {
        name.starts_with(&namespace)
            && !matches!(owners.get(name), Some(Some(owner)) if *owner != candidates.channel)
    }) {
        return Verdict::Hold("owner_unresolved");
    }
    if let Some(absence) = hosted {
        return Verdict::Absent(absence);
    }
    if candidates.current.is_none()
        && !(matches!(scope.result, Scope::LocalNameless)
            && candidates.names.is_empty()
            && names.is_empty())
    {
        return Verdict::Hold("candidate_unresolved");
    }
    Verdict::Absent(Absence::LocalInventory)
}

// Keep the existing read-only resolver adapter and its witnesses in one module.
fn observed_row(lookup: &HostedLookup) -> HostWitness {
    session_record_witness(lookup)
}
fn observed_marker(marker: HostKindMarker) -> HostWitness {
    marker_witness(marker)
}
fn raw_evidence(
    key: Option<String>,
    name: Option<String>,
    exact: &HostedLookup,
    marker: HostKindMarker,
    runtime: Option<crate::services::agent_protocol::RuntimeHandoffKind>,
    process: bool,
) -> SessionTargetEvidence {
    SessionTargetEvidence {
        session_key: key,
        session_name: name,
        session_record: observed_row(exact),
        inflight_locator: HostWitness::Absent,
        host_marker: observed_marker(marker),
        runtime_kind_marker: runtime,
        process_registry_hit: process,
        ..SessionTargetEvidence::unread()
    }
}
fn local_registration(config: &Config, channel: u64, provider: &ProviderKind) -> bool {
    let bindings: Vec<_> = config
        .agents
        .iter()
        .flat_map(|a| a.channels.iter())
        .filter(|(_, b)| b.channel_id().and_then(|id| id.parse::<u64>().ok()) == Some(channel))
        .collect();
    !bindings.is_empty()
        && bindings.iter().all(|(name, binding)| {
            let name = binding
                .provider()
                .unwrap_or_else(|| (*name).into())
                .trim()
                .to_ascii_lowercase();
            name == provider.as_str() && channel_effective_tui_request(config, binding, &name)
        })
        && BootSessionHosts::from_config(config)
            .is_ok_and(|boot| boot.herdr_endpoint(channel).is_none())
}

#[cfg(test)]
mod dormancy_tests;
#[cfg(test)]
#[path = "unbound/unit_tests.rs"]
mod tests;

mod cross_runtime_owners;
pub(super) mod herdr_bound;
pub(super) mod hold_leaf;
pub(super) mod live;
mod routine_owner;
