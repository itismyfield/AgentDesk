use crate::services::provider::ProviderKind;
use crate::services::tui_input::ledger::Presence;
#[cfg(test)]
use crate::services::tui_input::transition::mutant;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChannelKind {
    NonThread,
    Thread,
}

#[derive(Clone, Debug)]
pub(crate) struct RouteBinding {
    pub(crate) agent: String,
    pub(crate) channel: Option<u64>,
    pub(crate) original_provider: Option<ProviderKind>,
    pub(crate) writer_provider: Option<ProviderKind>,
}

// None means an unreadable or incomplete snapshot; Some(empty) proves there are no targets.
#[derive(Clone, Debug)]
pub(crate) struct RoutingSnapshot {
    pub(crate) channel: u64,
    pub(crate) channel_kind: Option<ChannelKind>,
    pub(crate) primary: Option<Vec<RouteBinding>>,
    pub(crate) alt: Option<Vec<RouteBinding>>,
    pub(crate) cc: Option<Vec<RouteBinding>>,
    pub(crate) cdx: Option<Vec<RouteBinding>>,
    pub(crate) overrides: Option<Vec<RouteBinding>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    UnsupportedSelection,
    SnapshotUnknown,
    ChannelKindUnknown,
    Thread,
    PrimaryNotUnique,
    BindingUnknown,
    PrimaryProviderMismatch,
    CrossProviderTarget,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScopeDecision {
    // A supported scope is not LedgerOpen or permission to submit.
    Candidate,
    // No responsibility exists: report input_scope_refused and keep effective Legacy.
    Refused(Refusal),
    // Existing or uncertain responsibility stays protected; Legacy fallback is forbidden.
    Held(Refusal),
}

// Responsibility is the collector's combined WAL/clear/handback probe, never current ownership.
pub(crate) fn evaluate(
    channel: u64,
    selected_provider: &str,
    snapshot: Option<&RoutingSnapshot>,
    responsibility: Presence,
) -> ScopeDecision {
    match prove(channel, selected_provider, snapshot) {
        Ok(()) => ScopeDecision::Candidate,
        Err(reason) if responsibility == Presence::Absent => ScopeDecision::Refused(reason),
        Err(reason) => ScopeDecision::Held(reason),
    }
}

fn prove(
    channel: u64,
    selected_provider: &str,
    snapshot: Option<&RoutingSnapshot>,
) -> Result<(), Refusal> {
    if channel == 0 || !matches!(selected_provider, "claude" | "codex") {
        return Err(Refusal::UnsupportedSelection);
    }
    let Some(snapshot) = snapshot else {
        #[cfg(test)]
        if mutant("g2-e2-unknown-pass") {
            return Ok(());
        }
        return Err(Refusal::SnapshotUnknown);
    };
    if snapshot.channel != channel {
        return Err(Refusal::SnapshotUnknown);
    }
    match snapshot.channel_kind {
        Some(ChannelKind::NonThread) => (),
        #[cfg(test)]
        Some(ChannelKind::Thread) if mutant("g2-e2-thread-pass") => (),
        Some(ChannelKind::Thread) => return Err(Refusal::Thread),
        None => return Err(Refusal::ChannelKindUnknown),
    }
    let (Some(primary), Some(alt), Some(cc), Some(cdx), Some(overrides)) = (
        &snapshot.primary,
        &snapshot.alt,
        &snapshot.cc,
        &snapshot.cdx,
        &snapshot.overrides,
    ) else {
        return Err(Refusal::SnapshotUnknown);
    };
    if [primary, alt, cc, cdx, overrides]
        .into_iter()
        .flatten()
        .any(|binding| binding.channel.is_none())
    {
        return Err(Refusal::BindingUnknown);
    }
    let mut primary = primary
        .iter()
        .filter(|binding| binding.channel == Some(channel));
    let binding = primary.next().ok_or(Refusal::PrimaryNotUnique)?;
    if primary.next().is_some() {
        return Err(Refusal::PrimaryNotUnique);
    }
    check_providers(binding, selected_provider, Refusal::PrimaryProviderMismatch)?;
    for bindings in [alt, cc, cdx, overrides] {
        #[cfg(test)]
        if std::ptr::eq(bindings, alt) && mutant("g2-e2-alt-pass") {
            continue;
        }
        for binding in bindings
            .iter()
            .filter(|binding| binding.channel == Some(channel))
        {
            check_providers(binding, selected_provider, Refusal::CrossProviderTarget)?;
        }
    }
    Ok(())
}

fn check_providers(
    binding: &RouteBinding,
    selected: &str,
    mismatch: Refusal,
) -> Result<(), Refusal> {
    if binding.agent.trim().is_empty() {
        return Err(Refusal::BindingUnknown);
    }
    for provider in [&binding.original_provider, &binding.writer_provider] {
        match provider {
            Some(ProviderKind::Unsupported(_)) | None => return Err(Refusal::BindingUnknown),
            Some(provider) if provider.as_str() != selected => return Err(mismatch),
            Some(_) => (),
        }
    }
    Ok(())
}
