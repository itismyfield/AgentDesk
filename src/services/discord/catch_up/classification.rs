use super::health::UtilityBotUserIdResolution;

/// Eligible/rejection buckets for catch-up scans. These are logged separately so
/// "no recovery" is distinguishable from filter, dedupe, and age-window skips.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum CatchUpClassification {
    /// Eligible user/allowed-bot message that should be enqueued.
    Recover,
    /// System message kind (thread-created / slash-command etc.) - silently dropped.
    SystemKind,
    /// Authored by this bot (self) - must not re-enqueue our own output.
    SelfAuthored,
    /// Already present in the live mailbox / known set - duplicate.
    Duplicate,
    /// Older than the catch-up max-age window - too late to safely replay.
    TooOld,
    /// #4564: this inbound message already has a CONFIRMED terminal delivery on
    /// the durable completed-turn ledger. Suppresses the false restart-gap
    /// TooOld notice without touching the DLQ path. Positioned strictly between
    /// the sender-eligibility gates and the age gate so it never overrides
    /// NotAllowed/SelfAuthored (#4443/#4453) and only pre-empts TooOld.
    Settled,
    /// Empty content (whitespace only).
    Empty,
    /// Authored by a non-allowed bot or an allowed bot without DISPATCH prefix.
    NotAllowed,
}

/// Per-channel running tally of [`CatchUpClassification`] outcomes - fed into
/// the always-on breakdown log. Keeping this separate from the recovery loop
/// keeps the filter-stats accounting honest and unit-testable.
#[derive(Debug, Default, Clone, Copy)]
pub(in crate::services::discord) struct CatchUpScanStats {
    pub returned: usize,
    pub recovered: usize,
    pub system_kind: usize,
    pub self_authored: usize,
    pub duplicate: usize,
    pub too_old: usize,
    /// #4564: aged messages suppressed by the durable completed-turn ledger.
    /// Tallied separately from `duplicate` so ledger suppression is observable
    /// and never conflated with live-mailbox dedup.
    pub settled: usize,
    pub empty: usize,
    pub not_allowed: usize,
}

impl CatchUpScanStats {
    pub(in crate::services::discord) fn record(&mut self, outcome: CatchUpClassification) {
        match outcome {
            CatchUpClassification::Recover => self.recovered += 1,
            CatchUpClassification::SystemKind => self.system_kind += 1,
            CatchUpClassification::SelfAuthored => self.self_authored += 1,
            CatchUpClassification::Duplicate => self.duplicate += 1,
            CatchUpClassification::TooOld => self.too_old += 1,
            CatchUpClassification::Settled => self.settled += 1,
            CatchUpClassification::Empty => self.empty += 1,
            CatchUpClassification::NotAllowed => self.not_allowed += 1,
        }
    }
}

/// Plain inputs to the catch-up filter, decoupled from `serenity::Message` so
/// the classification order can be tested without a Discord runtime.
#[derive(Debug, Clone)]
pub(in crate::services::discord) struct CatchUpMessageView {
    pub message_id: u64,
    pub author_id: u64,
    pub author_is_bot: bool,
    pub is_processable_kind: bool,
    pub age_secs: i64,
    pub trimmed_text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum CatchUpClassificationDecision {
    Determinate(CatchUpClassification),
    UtilityIdentityUnavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CatchUpDisposition {
    outcome: CatchUpClassification,
    actionable_too_old: bool,
}

/// Pure phase-1 filter. Sender eligibility, including the output-only notify
/// identity, is decided before the age gate so non-input automation cannot
/// become recovery work or TooOld/DLQ evidence.
pub(in crate::services::discord) fn classify_catch_up_message(
    msg: &CatchUpMessageView,
    bot_user_id: Option<u64>,
    existing_ids: &std::collections::HashSet<u64>,
    settled_ids: &std::collections::HashSet<u64>,
    max_age_secs: i64,
    allowed_bot_ids: &[u64],
    announce_bot_id: Option<u64>,
    notify_bot_id: Option<u64>,
) -> CatchUpClassification {
    if !msg.is_processable_kind {
        return CatchUpClassification::SystemKind;
    }
    if Some(msg.author_id) == bot_user_id {
        return CatchUpClassification::SelfAuthored;
    }
    if existing_ids.contains(&msg.message_id) {
        return CatchUpClassification::Duplicate;
    }
    if super::is_restart_gap_notice(msg.author_is_bot, &msg.trimmed_text) {
        return CatchUpClassification::SelfAuthored;
    }
    if msg.trimmed_text.is_empty() {
        return CatchUpClassification::Empty;
    }
    if Some(msg.author_id) == notify_bot_id {
        return CatchUpClassification::NotAllowed;
    }
    if !super::is_allowed_turn_sender(
        allowed_bot_ids,
        announce_bot_id,
        msg.author_id,
        msg.author_is_bot,
        &msg.trimmed_text,
    ) {
        return CatchUpClassification::NotAllowed;
    }
    // #4564: a confirmed terminal delivery on the durable ledger settles this
    // inbound message. Placed AFTER every sender-eligibility gate
    // (SystemKind/SelfAuthored/Duplicate/is_restart_gap_notice/Empty/NotAllowed)
    // so ledger consult can never override them (#4443/#4453 invariant), and
    // BEFORE the age gate so an already-answered aged message is Settled rather
    // than TooOld — the #4564 fix. Membership is keyed strictly by `message_id`,
    // so a message NOT on the ledger falls straight through to the age gate.
    if settled_ids.contains(&msg.message_id) {
        return CatchUpClassification::Settled;
    }
    if msg.age_secs > max_age_secs {
        return CatchUpClassification::TooOld;
    }
    CatchUpClassification::Recover
}

pub(super) fn too_old_is_actionable(
    outcome: CatchUpClassification,
    author_id: u64,
    author_is_bot: bool,
    allowed_bot_ids: &[u64],
    announce_bot_id: Option<u64>,
    notify_bot_id: Option<u64>,
) -> bool {
    outcome == CatchUpClassification::TooOld
        && !author_is_bot
        && !allowed_bot_ids.contains(&author_id)
        && announce_bot_id != Some(author_id)
        && notify_bot_id != Some(author_id)
}

fn disposition_for_utility_ids(
    msg: &CatchUpMessageView,
    bot_user_id: Option<u64>,
    existing_ids: &std::collections::HashSet<u64>,
    settled_ids: &std::collections::HashSet<u64>,
    max_age_secs: i64,
    allowed_bot_ids: &[u64],
    announce_bot_id: Option<u64>,
    notify_bot_id: Option<u64>,
) -> CatchUpDisposition {
    let outcome = classify_catch_up_message(
        msg,
        bot_user_id,
        existing_ids,
        settled_ids,
        max_age_secs,
        allowed_bot_ids,
        announce_bot_id,
        notify_bot_id,
    );
    CatchUpDisposition {
        outcome,
        actionable_too_old: too_old_is_actionable(
            outcome,
            msg.author_id,
            msg.author_is_bot,
            allowed_bot_ids,
            announce_bot_id,
            notify_bot_id,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn phase2_disposition_for_utility_ids(
    msg: &CatchUpMessageView,
    bot_user_id: Option<u64>,
    existing_ids: &std::collections::HashSet<u64>,
    settled_ids: &std::collections::HashSet<u64>,
    max_age_secs: i64,
    allowed_bot_ids: &[u64],
    announce_bot_id: Option<u64>,
    notify_bot_id: Option<u64>,
    author_is_authorized: bool,
) -> CatchUpDisposition {
    let mut disposition = disposition_for_utility_ids(
        msg,
        bot_user_id,
        existing_ids,
        settled_ids,
        max_age_secs,
        allowed_bot_ids,
        announce_bot_id,
        notify_bot_id,
    );
    let is_allowed_automation = allowed_bot_ids.contains(&msg.author_id)
        || announce_bot_id.is_some_and(|id| id == msg.author_id);
    if disposition.outcome == CatchUpClassification::Recover
        && !is_allowed_automation
        && !author_is_authorized
    {
        disposition.outcome = CatchUpClassification::NotAllowed;
    }
    disposition
}

fn decision_for_utility_resolution(
    msg: &CatchUpMessageView,
    announce_resolution: UtilityBotUserIdResolution,
    notify_resolution: UtilityBotUserIdResolution,
    disposition_for_ids: impl Fn(Option<u64>, Option<u64>) -> CatchUpDisposition,
) -> CatchUpClassificationDecision {
    let announce_bot_id = announce_resolution.user_id();
    let notify_bot_id = notify_resolution.user_id();
    let observed = disposition_for_ids(announce_bot_id, notify_bot_id);

    let announce_alternative =
        matches!(announce_resolution, UtilityBotUserIdResolution::Unavailable)
            .then(|| disposition_for_ids(Some(msg.author_id), notify_bot_id));
    let notify_alternative = matches!(notify_resolution, UtilityBotUserIdResolution::Unavailable)
        .then(|| disposition_for_ids(announce_bot_id, Some(msg.author_id)));

    if announce_alternative.is_some_and(|alternative| alternative != observed)
        || notify_alternative.is_some_and(|alternative| alternative != observed)
    {
        CatchUpClassificationDecision::UtilityIdentityUnavailable
    } else {
        CatchUpClassificationDecision::Determinate(observed.outcome)
    }
}

/// Classify one message without turning a transient utility-bot lookup failure
/// into an irreversible checkpoint advance.
///
/// For each unavailable identity, compare the observed disposition with the
/// disposition if that identity belonged to the current author. This includes
/// the user-facing TooOld notice bit, not just the enum outcome: an aged
/// false-flag announce message remains `TooOld` either way, but must not be
/// surfaced as something a human can resend. Only a semantic difference is
/// deferred, so a stable legacy card or known non-actionable bot does not enter
/// an identity retry loop merely because an unrelated utility lookup is down.
pub(in crate::services::discord) fn classify_catch_up_message_with_utility_resolution(
    msg: &CatchUpMessageView,
    bot_user_id: Option<u64>,
    existing_ids: &std::collections::HashSet<u64>,
    settled_ids: &std::collections::HashSet<u64>,
    max_age_secs: i64,
    allowed_bot_ids: &[u64],
    announce_resolution: UtilityBotUserIdResolution,
    notify_resolution: UtilityBotUserIdResolution,
) -> CatchUpClassificationDecision {
    decision_for_utility_resolution(
        msg,
        announce_resolution,
        notify_resolution,
        |announce_bot_id, notify_bot_id| {
            disposition_for_utility_ids(
                msg,
                bot_user_id,
                existing_ids,
                settled_ids,
                max_age_secs,
                allowed_bot_ids,
                announce_bot_id,
                notify_bot_id,
            )
        },
    )
}

/// Phase-2 counterpart to [`classify_catch_up_message_with_utility_resolution`].
/// In addition to sender classification, this includes the announce identity's
/// authorization-bypass semantics. Without that extra disposition bit, a
/// false-flag announce message can look like an ordinary unauthorized human
/// while the utility lookup is down and be irreversibly skipped.
#[allow(clippy::too_many_arguments)]
pub(in crate::services::discord) fn classify_phase2_message_with_utility_resolution(
    msg: &CatchUpMessageView,
    bot_user_id: Option<u64>,
    existing_ids: &std::collections::HashSet<u64>,
    settled_ids: &std::collections::HashSet<u64>,
    max_age_secs: i64,
    allowed_bot_ids: &[u64],
    announce_resolution: UtilityBotUserIdResolution,
    notify_resolution: UtilityBotUserIdResolution,
    author_is_authorized: bool,
) -> CatchUpClassificationDecision {
    decision_for_utility_resolution(
        msg,
        announce_resolution,
        notify_resolution,
        |announce_bot_id, notify_bot_id| {
            phase2_disposition_for_utility_ids(
                msg,
                bot_user_id,
                existing_ids,
                settled_ids,
                max_age_secs,
                allowed_bot_ids,
                announce_bot_id,
                notify_bot_id,
                author_is_authorized,
            )
        },
    )
}
