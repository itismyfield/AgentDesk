use crate::services::discord::outbound::{census_scope, current_census};
use serde_json::{Value, json};
use std::time::Instant;

use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::tui_o::{
    alarm::AlarmRouter,
    channel_policy::{self, BodyClaimed, BootChannels, ClaimReason, LegacySend, SendStage},
    writer::WriterAlarm,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum IdentityError {
    #[error("writer boot snapshot is unavailable")]
    MissingSnapshot,
    #[error("Discord destination channel is unknown")]
    UnknownChannel,
    #[error("selected channel runtime kind is unknown")]
    UnknownKind,
    #[error("selected channel runtime kind {actual:?} differs from boot kind {expected:?}")]
    KindMismatch {
        expected: Option<RuntimeHandoffKind>,
        actual: RuntimeHandoffKind,
    },
    #[error("TUI-bound channel is outside the O writer selection")]
    UnselectedTui,
    #[error("thread child has no resolved parent policy or decided adoption")]
    ChildHeld,
    #[error("thread writer is waiting for its verified source and delivery fence")]
    ChildDeliveryHeld,
}

/// How a caller uses the answer: before a body Legacy would send, or only to read ownership.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Use {
    /// A pending adoption is released: Legacy takes the channel for this process.
    Body,
    /// A pending adoption reads as Legacy and stays pending.
    Peek,
}

#[derive(Default, Clone, Copy)]
struct Evidence<'a> {
    session: Option<&'a str>,
    nonce: Option<[u8; 32]>,
    source: Option<[u8; 32]>,
    reserved: bool,
}

/// The raw claim behind [`claim_then_send`]: a pending adoption is released to Legacy, whose send
/// stays counted until the answer's guard drops. Callers outside it are pinned in the census.
#[track_caller]
pub(crate) fn o_owns_tui_output_for_channel_reserving(
    channel_id: u64,
    kind: Option<RuntimeHandoffKind>,
) -> Result<BodyClaimed, IdentityError> {
    decide_from(
        channel_id,
        Use::Body,
        Some(std::panic::Location::caller()),
        Evidence::default(),
        || kind,
    )
}

/// Test builds: the raw claim's answer alone, its send counted as finished at once.
#[cfg(test)]
pub(crate) fn o_owns_tui_output_for_channel(
    channel_id: u64,
    kind: Option<RuntimeHandoffKind>,
) -> Result<bool, IdentityError> {
    o_owns_tui_output_for_channel_reserving(channel_id, kind).map(|claimed| claimed.owned)
}

#[cfg(test)]
pub(crate) fn o_owns_tui_output_for_channel_tmux(
    channel_id: u64,
    session: Option<&str>,
) -> Result<bool, IdentityError> {
    BodyClaim::tmux(channel_id, session)
        .judge(Use::Body)
        .map(|claimed| claimed.owned)
}

/// For diagnostics and lifecycle checks that send no body: a pending adoption stays pending.
pub(crate) fn peek_o_owns_tui_output_for_channel(
    channel_id: u64,
    kind: Option<RuntimeHandoffKind>,
) -> Result<bool, IdentityError> {
    decide(channel_id, Use::Peek, || kind).map(|peeked| peeked.owned)
}

/// Observe capture eligibility without raising a writer alarm when no body is offered.
#[cfg(unix)]
pub(crate) fn peek_o_owns_tui_output_for_channel_quiet(
    channel_id: u64,
    kind: Option<RuntimeHandoffKind>,
) -> Result<bool, IdentityError> {
    super::test_override::with_channels(|snapshot| {
        decide_with_snapshot(
            snapshot,
            channel_id,
            Use::Peek,
            None,
            Evidence::default(),
            || kind,
        )
    })
    .map(|peeked| peeked.owned)
}

pub(crate) fn peek_o_owns_tui_output_for_channel_tmux(
    channel_id: u64,
    session: Option<&str>,
) -> Result<bool, IdentityError> {
    BodyClaim::tmux(channel_id, session).peek()
}

/// Where a Legacy body goes, with how the destination's runtime kind is found.
#[derive(Clone, Copy)]
pub(crate) struct BodyClaim<'a> {
    channel_id: u64,
    kind: KindOf<'a>,
    direct: bool,
    nonce: Option<[u8; 32]>,
    source: Option<[u8; 32]>,
    reserved: bool,
    origin: &'static std::panic::Location<'static>,
}

#[derive(Clone, Copy)]
enum KindOf<'a> {
    Known(Option<RuntimeHandoffKind>),
    Tmux(Option<&'a str>),
}

impl<'a> BodyClaim<'a> {
    #[track_caller]
    pub(crate) fn new(channel_id: u64, kind: Option<RuntimeHandoffKind>) -> Self {
        let kind = KindOf::Known(kind);
        Self {
            channel_id,
            kind,
            direct: true,
            nonce: None,
            source: None,
            reserved: false,
            origin: std::panic::Location::caller(),
        }
    }

    #[track_caller]
    pub(crate) fn tmux(channel_id: u64, session: Option<&'a str>) -> Self {
        let kind = KindOf::Tmux(session);
        Self {
            channel_id,
            kind,
            direct: true,
            nonce: None,
            source: None,
            reserved: false,
            origin: std::panic::Location::caller(),
        }
    }

    pub(crate) fn for_turn(self, nonce: Option<&str>) -> Self {
        Self {
            nonce: nonce.map(channel_policy::first_nonce_id),
            ..self
        }
    }

    pub(crate) fn for_source(self, path: Option<&str>) -> Self {
        Self {
            source: path.map(channel_policy::first_nonce_id),
            ..self
        }
    }

    /// Pin a row's reserved launch before dropping the row used to create this offer.
    pub(crate) fn for_tmux_session(self, session: Option<&str>) -> Self {
        let reserved = session
            .is_some_and(|session| super::claude_thread::holds_tmux_body(self.channel_id, session));
        Self { reserved, ..self }
    }

    pub(crate) fn peek(self) -> Result<bool, IdentityError> {
        self.judge(Use::Peek).map(|claimed| claimed.owned)
    }

    pub(crate) fn peek_o_owns_tui_output_for_channel(self) -> Result<bool, IdentityError> {
        self.peek()
    }

    /// A caller without a direct gateway; see [`o_keeps_body`].
    pub(crate) fn direct(self, direct: bool) -> Self {
        Self { direct, ..self }
    }

    fn judge(self, usage: Use) -> Result<BodyClaimed, IdentityError> {
        let channel_id = self.channel_id;
        let session = match self.kind {
            KindOf::Known(_) => None,
            KindOf::Tmux(session) => session,
        };
        decide_from(
            channel_id,
            usage,
            Some(self.origin),
            Evidence {
                session,
                nonce: self.nonce,
                source: self.source,
                reserved: self.reserved,
            },
            || match self.kind {
                KindOf::Known(kind) => kind,
                KindOf::Tmux(session) => session_kind(channel_id, session),
            },
        )
    }

    fn claim(self) -> Result<BodyClaimed, IdentityError> {
        let claimed = self.judge(Use::Body)?;
        let owned = o_keeps_body(self.channel_id, claimed.owned, self.direct);
        Ok(BodyClaimed { owned, ..claimed })
    }
}

/// An owned channel's body is O's whatever the caller's gateway. A caller without a direct one
/// alarms the channel when this process's writer is not taking it, as that body waits for O.
pub(crate) fn o_keeps_body(channel_id: u64, owned: bool, direct: bool) -> bool {
    if owned && !direct && !super::intake_route::accepts(channel_id) {
        let detail = "writer is not taking a body from a caller without a direct gateway".into();
        let alarm = WriterAlarm::Halted { detail };
        AlarmRouter::for_process(None, None).raise_at(channel_id, &alarm, Instant::now());
    }
    owned
}

/// What became of a body offered to [`claim_then_send`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum BodySend<T> {
    /// O owns the channel, so nothing was sent.
    OwnedByO,
    Sent(T),
}

impl<T> BodySend<Result<T, String>> {
    /// The send's own result, with O owning the channel or a held identity as a failed send.
    pub(crate) fn flatten(sent: Result<Self, IdentityError>) -> Result<T, String> {
        match sent {
            Ok(Self::Sent(result)) => result,
            Ok(Self::OwnedByO) => Err("O owns this channel's body".to_string()),
            Err(error) => Err(format!("TUI output identity held: {error}")),
        }
    }
}

/// The one place a Legacy body ends a pending adoption: claim, then send at once unless O owns
/// the channel. Callers settle guards and no-ops on a peek first; `None` sends without a claim.
/// The claimed send stays counted until `send` completes.
pub(crate) async fn claim_then_send<T, F: std::future::Future<Output = T>>(
    claim: Option<BodyClaim<'_>>,
    send: impl FnOnce() -> F,
) -> Result<BodySend<T>, IdentityError> {
    let (sent, held) = claim_then_send_held(claim, send).await?;
    if let Some(held) = &held {
        held.reached(SendStage::SentUnsettled);
    }
    Ok(sent)
}

/// [`claim_then_send`] for a body sent in more than one step: the claimed send is handed back
/// so the caller keeps it counted until its last step is done.
pub(crate) async fn claim_then_send_held<T, F: std::future::Future<Output = T>>(
    claim: Option<BodyClaim<'_>>,
    send: impl FnOnce() -> F,
) -> Result<(BodySend<T>, Option<LegacySend>), IdentityError> {
    if let Some(claim) = claim {
        super::wait_codex_first_id(claim.channel_id, claim.nonce).await;
    }
    let claimed = reserve_body(claim)?;
    if claimed.owned {
        return Ok((BodySend::OwnedByO, None));
    }
    Ok((BodySend::Sent(send().await), claimed.send))
}

#[derive(Clone, Copy, serde::Serialize)]
pub(crate) enum BodySite {
    #[serde(rename = "W01")]
    WatcherTerminal,
    #[serde(rename = "W01a")]
    WatcherDirectFallback,
    #[serde(rename = "W01b")]
    WatcherReplace,
    #[serde(rename = "W01c")]
    WatcherPostFallback,
    #[serde(rename = "W01d")]
    WatcherShortController,
    #[serde(rename = "W01e")]
    WatcherLongController,
    #[serde(rename = "W01f")]
    WatcherLongChunks,
    #[serde(rename = "W02")]
    WatcherStreaming,
    #[serde(rename = "W02b")]
    WatcherProviderGuard,
    #[serde(rename = "FOOTER-INITIAL")]
    FooterInitial,
    #[serde(rename = "FOOTER-STRIP")]
    FooterStrip,
    #[serde(rename = "FOOTER-REFRESH")]
    FooterRefresh,
    #[serde(rename = "W06")]
    StandbyRelay,
    #[serde(rename = "W10")]
    BridgeTerminal,
    #[serde(rename = "W10b")]
    BridgeShortController,
    #[serde(rename = "W10c")]
    BridgeLongController,
    #[serde(rename = "W10d")]
    BridgeLongChunks,
    #[serde(rename = "W10e")]
    BridgePinnedTerminal,
    #[serde(rename = "W11")]
    BridgeCancelled,
    #[serde(rename = "W13")]
    BridgeForeignTerminal,
    #[serde(rename = "W14")]
    BridgeStreaming,
    #[serde(rename = "W14a")]
    BridgeRollover,
    #[serde(rename = "W14b")]
    BridgeRolloverRestore,
    #[serde(rename = "W14c")]
    BridgeBlockedRollover,
    #[serde(rename = "W17")]
    BridgeHeadless,
    #[serde(rename = "W19")]
    OutputController,
    #[serde(rename = "W30")]
    RecoveryNotice,
    #[serde(rename = "W30a")]
    RecoveryController,
    #[serde(rename = "W31")]
    RecoveredTerminal,
    #[serde(rename = "W32")]
    RecoveryChunks,
    #[serde(rename = "W33")]
    HealthRecovery,
    #[serde(rename = "W34")]
    RestartHandoff,
    #[serde(rename = "W35")]
    RestartAnchor,
    #[serde(rename = "W20")]
    SessionSink,
    #[serde(rename = "W20a")]
    SessionShortController,
    #[serde(rename = "W20b")]
    SessionChunks,
    #[serde(rename = "W20d")]
    SessionTaskResponse,
    #[serde(rename = "W21")]
    TaskResponse,
    #[serde(rename = "W01g")]
    WatcherTaskResponse,
}

/// Observation remains separate from a Legacy send guard, including unclaimed transports.
pub(crate) struct BodyTransport {
    _send: Option<LegacySend>,
    tag: Option<Value>,
}

impl BodyTransport {
    fn inherited(site: BodySite, send: Option<LegacySend>, reason: ClaimReason) -> Self {
        let mut tag = current_census();
        if let Some(object) = tag.as_mut().and_then(Value::as_object_mut) {
            if !object.contains_key("site") {
                object.insert("site".into(), json!(site));
            }
            if !object.contains_key("reason") {
                object.insert("reason".into(), json!(reason));
            }
        }
        Self { _send: send, tag }
    }

    /// The holder's report of how far its claimed send got; an unclaimed transport has none.
    pub(crate) fn reached(&self, stage: SendStage) {
        if let Some(send) = &self._send {
            send.reached(stage);
        }
    }

    pub(crate) async fn run<T>(&self, next: impl std::future::Future<Output = T>) -> T {
        if self.tag.is_some() {
            census_scope(self.tag.clone(), next).await
        } else {
            next.await
        }
    }
}

pub(crate) async fn claim_then_send_at<T, F: std::future::Future<Output = T>>(
    site: BodySite,
    claim: Option<BodyClaim<'_>>,
    send: impl FnOnce() -> F,
) -> Result<BodySend<T>, IdentityError> {
    let (sent, context) = claim_then_send_held_at(site, claim, send).await?;
    context.reached(SendStage::SentUnsettled);
    Ok(sent)
}

pub(crate) async fn claim_then_send_held_at<T, F: std::future::Future<Output = T>>(
    site: BodySite,
    claim: Option<BodyClaim<'_>>,
    send: impl FnOnce() -> F,
) -> Result<(BodySend<T>, BodyTransport), IdentityError> {
    if let Some(claim) = claim {
        super::wait_codex_first_id(claim.channel_id, claim.nonce).await;
    }
    let claimed = reserve_body(claim)?;
    let context = BodyTransport::inherited(site, claimed.send, claimed.reason);
    if claimed.owned {
        return Ok((BodySend::OwnedByO, context));
    }
    let sent = context.run(async { send().await }).await;
    Ok((BodySend::Sent(sent), context))
}

fn reserve_body(claim: Option<BodyClaim<'_>>) -> Result<BodyClaimed, IdentityError> {
    let Some(claim) = claim else {
        return Ok(legacy(ClaimReason::NoClaim));
    };
    let claimed = claim.claim()?;
    #[cfg(test)]
    CLAIMS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push((claim.channel_id, claimed.owned));
    Ok(claimed)
}

/// For a stuck report: the Legacy send counters and oldest open send of the adoption a body
/// claim on `channel_id` counts against; none when no adoption is selected for it.
pub(crate) fn legacy_sends_report(channel_id: u64) -> Option<String> {
    super::test_override::with_channels(|snapshot| {
        let snapshot = snapshot?;
        let candidate = snapshot
            .unresolved_child(channel_id)
            .or_else(|| Some(snapshot.child(channel_id)?.ok()?.candidate))
            .or_else(|| snapshot.candidate(channel_id).cloned())?;
        Some(candidate.sends_report())
    })
}

/// Test builds: every body claim this process judged, with whether O took the body.
#[cfg(test)]
static CLAIMS: std::sync::Mutex<Vec<(u64, bool)>> = std::sync::Mutex::new(Vec::new());

/// The body claims judged for `channel` so far, true where O took the body.
#[cfg(test)]
pub(crate) fn claims_judged(channel: u64) -> Vec<bool> {
    let claims = CLAIMS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let judged = claims.iter().filter(|(c, _)| *c == channel);
    judged.map(|(_, owned)| *owned).collect()
}

fn session_kind(channel: u64, session: Option<&str>) -> Option<RuntimeHandoffKind> {
    session.and_then(|session| {
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(session)
            .map(|binding| binding.runtime_kind)
            .or_else(|| crate::services::tmux_common::resolve_tmux_runtime_kind_marker(session))
            .or_else(|| {
                super::claude_thread::held_runtime_context(channel, session)
                    .then_some(RuntimeHandoffKind::ClaudeTui)
            })
    })
}

fn decide(
    channel_id: u64,
    usage: Use,
    resolve_kind: impl FnOnce() -> Option<RuntimeHandoffKind>,
) -> Result<BodyClaimed, IdentityError> {
    decide_from(channel_id, usage, None, Evidence::default(), resolve_kind)
}

fn decide_from(
    channel_id: u64,
    usage: Use,
    origin: Option<&'static std::panic::Location<'static>>,
    evidence: Evidence<'_>,
    resolve_kind: impl FnOnce() -> Option<RuntimeHandoffKind>,
) -> Result<BodyClaimed, IdentityError> {
    let evaluate = |snapshot: Option<&BootChannels>| {
        decide_with_snapshot(snapshot, channel_id, usage, origin, evidence, resolve_kind)
    };
    #[cfg(test)]
    let result = super::test_override::with_channels(evaluate);
    #[cfg(not(test))]
    let result = evaluate(channel_policy::boot());
    result.map_err(|error| error.hold(channel_id))
}

impl IdentityError {
    pub(crate) fn hold(self, channel_id: u64) -> Self {
        let alarm = WriterAlarm::Halted {
            detail: self.to_string(),
        };
        AlarmRouter::for_process(None, None).raise_at(channel_id, &alarm, Instant::now());
        if self != Self::ChildDeliveryHeld {
            tracing::error!(channel_id, reason = ?ClaimReason::IdentityHeld, error = %self, "tui_o output identity held");
        }
        self
    }
}

/// A channel outside the selection is Legacy's unless the boot config binds it as a TUI, whose
/// body is held rather than sent by Legacy.
fn outside_selection(
    snapshot: &BootChannels,
    channel_id: u64,
    reason: ClaimReason,
) -> Result<BodyClaimed, IdentityError> {
    if snapshot.tui_bound(channel_id) {
        return Err(IdentityError::UnselectedTui);
    }
    Ok(legacy(reason))
}

/// Legacy's answer for a channel with no adoption to count its send against.
fn legacy(reason: ClaimReason) -> BodyClaimed {
    BodyClaimed {
        owned: false,
        send: None,
        reason,
    }
}

fn decide_with_snapshot(
    snapshot: Option<&BootChannels>,
    channel_id: u64,
    usage: Use,
    origin: Option<&'static std::panic::Location<'static>>,
    evidence: Evidence<'_>,
    resolve_kind: impl FnOnce() -> Option<RuntimeHandoffKind>,
) -> Result<BodyClaimed, IdentityError> {
    let snapshot = snapshot.ok_or(IdentityError::MissingSnapshot)?;
    // A verified empty list is O off: decided before any destination or kind is resolved.
    if snapshot.channels().is_empty() {
        return outside_selection(snapshot, channel_id, ClaimReason::EmptySelection);
    }
    if channel_id == 0 {
        return Err(IdentityError::UnknownChannel);
    }
    // The body and its host share both state and sends.
    let judged = |candidate: &channel_policy::Candidate| match usage {
        Use::Body => match origin {
            Some(origin) => candidate.claim_body_from(channel_id, origin),
            None => candidate.claim_body(channel_id),
        },
        Use::Peek => {
            let state = candidate.peek();
            BodyClaimed {
                owned: state.owned(),
                send: None,
                reason: if state == channel_policy::Adoption::Pending {
                    ClaimReason::NoCandidate
                } else {
                    state.claim_reason()
                },
            }
        }
    };
    // The inner None is a boot child whose parent has yet to resolve.
    let fenced = |candidate: &channel_policy::Candidate, thread: Option<Option<u64>>| {
        let fence = (matches!(usage, Use::Body) && (thread.is_some() || evidence.reserved))
            .then(|| candidate.lock_thread_body());
        if fence.is_some()
            && !candidate.peek().owned()
            && (evidence.reserved
                || super::claude_thread::resume_fenced(
                    channel_id,
                    evidence.session,
                    thread.flatten(),
                ))
            && !super::claude_thread::names_legacy_turn(
                channel_id,
                candidate,
                evidence.nonce,
                evidence.source,
            )
        {
            return Err(IdentityError::ChildDeliveryHeld);
        }
        Ok(judged(candidate))
    };
    // A boot reservation with no parent read yet stays Legacy's; its sends gate the later handoff.
    if let Some(reserved) = snapshot.unresolved_child(channel_id) {
        if evidence.reserved
            || super::claude_thread::holds_body(snapshot, channel_id, evidence.session)
        {
            return Err(IdentityError::ChildDeliveryHeld);
        }
        return fenced(&reserved, Some(None));
    }
    // The parent selects the policy. A known thread offers the kind of its own execution.
    let child = match snapshot.child(channel_id) {
        Some(child) => Some(child.map_err(|_| IdentityError::ChildHeld)?),
        None => {
            // Membership comes before runtime lookup so unrelated Legacy channels stay independent.
            if !snapshot.channels().contains(&channel_id)
                && (evidence.reserved
                    || (evidence.session.is_some()
                        && super::claude_thread::holds_body(
                            snapshot,
                            channel_id,
                            evidence.session,
                        )))
            {
                return Err(IdentityError::ChildDeliveryHeld);
            }
            if !snapshot.channels().contains(&channel_id) {
                return outside_selection(snapshot, channel_id, ClaimReason::NonMember);
            }
            None
        }
    };
    let expected = snapshot.kind(channel_id);
    if expected == Some(RuntimeHandoffKind::ClaudeTui)
        && let Some(child) = child.as_ref()
        && !child.candidate.peek().owned()
        && (evidence.reserved
            || super::claude_thread::holds_body(snapshot, channel_id, evidence.session))
        && !super::claude_thread::names_legacy_turn(
            channel_id,
            &child.candidate,
            evidence.nonce,
            evidence.source,
        )
    {
        return Err(IdentityError::ChildDeliveryHeld);
    }
    // A committed member without a configured kind keeps Legacy out until its binding returns.
    if expected.is_none() {
        return Ok(BodyClaimed {
            owned: true,
            send: None,
            reason: ClaimReason::OwnedHeld,
        });
    }
    let kind = resolve_kind().ok_or(IdentityError::UnknownKind)?;
    if expected != Some(kind) {
        return Err(IdentityError::KindMismatch {
            expected,
            actual: kind,
        });
    }
    // A member's boot kind is always a TUI kind, so from here the channel is selected.
    // A late child keeps Legacy's open turn, counted, until its own adoption commits.
    let late_child = child
        .as_ref()
        .is_some_and(|child| child.body_owner() == Some(false));
    if let Some(execution) = snapshot.execution(channel_id)
        && (execution.permit.is_err() || !late_child)
    {
        return Ok(BodyClaimed {
            owned: true,
            send: None,
            reason: if execution.permit.is_err() {
                ClaimReason::OwnedHeld
            } else {
                ClaimReason::OwnedByO
            },
        });
    }
    // A child has its own adoption; an undecided one is held, as a claim would release it.
    if let Some(child) = child {
        child.body_owner().ok_or(IdentityError::ChildHeld)?;
        let parent = (child.kind == RuntimeHandoffKind::ClaudeTui).then_some(Some(child.parent));
        return fenced(&child.candidate, parent);
    }
    let Some(candidate) = snapshot.candidate(channel_id) else {
        return Ok(legacy(ClaimReason::NoCandidate));
    };
    fenced(candidate, None)
}

#[cfg(test)]
mod tests;
