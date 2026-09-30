//! Binding observation of a hook, judged before the hook has any other effect. A hook whose
//! binding evidence is not durable is refused with 425 so its sender retries it.

use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::services::tui_prompt_dedupe::binding_context::{BINDING_HEADER, decode_binding_header};
pub(crate) use crate::services::tui_prompt_dedupe::pane_registration::note_claude_pane_registration;
use crate::services::tui_prompt_dedupe::pane_registration::pane_registration_failed;
use axum::Json;
use axum::http::{HeaderMap, StatusCode};
use serde_json::{Value, json};

use super::HookEventKind;
use super::adoption_retry::{self, AdoptionHttp, DurableKind, NotDurableReason};
use super::relay_receipts::{RelayReceiptLedger, RelayReceiptTicket};
use crate::services::tui_prompt_dedupe::AdoptSkip;
use crate::services::tui_prompt_dedupe::binding_events::HookSignal;

/// Without a listed tmux, unmapped hooks stop being refused this long after the receiver starts.
const DISCOVERY_GRACE: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IngressOutcome {
    Proceed(ProceedReason),
    Durable(DurableKind),
    NotApplicable(NotApplicableReason),
    Unavailable(UnavailableReason),
    NotDurable(NotDurableReason),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProceedReason {
    NoSessionSwitch,
    NoChannelLog,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NotApplicableReason {
    CodexContextUnavailable,
    CodexSourceRejected,
    OtherProvider,
    PayloadNotUuid,
    NotClaudeTui,
    MalformedBindingPath,
    OlderThanBound,
    UnmappedCommandSession,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UnavailableReason {
    RestoreNotReady,
    PaneRegistrationFailed,
    MtimeUnreadable,
    ChannelNotRestored,
}

impl IngressOutcome {
    pub(crate) fn refused(self) -> bool {
        matches!(self, Self::Unavailable(_) | Self::NotDurable(_))
    }
}

static DISCOVERY_DONE: AtomicBool = AtomicBool::new(false);
static GRACE_WARNED: AtomicBool = AtomicBool::new(false);
static RECEIVER_STARTED: LazyLock<Instant> = LazyLock::new(Instant::now);
static UNMAPPED_COMMAND_SESSIONS: AtomicU64 = AtomicU64::new(0);
static LEGACY_NOT_DURABLE: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
thread_local! {
    static TEST_DISCOVERY_CLOCK: std::cell::Cell<(bool, Duration)> = const { std::cell::Cell::new((true, Duration::ZERO)) };
}

pub(crate) fn note_receiver_start() {
    LazyLock::force(&RECEIVER_STARTED);
}

/// A rehydrate pass that listed tmux has finished. It says nothing about any single pane.
pub(crate) fn mark_boot_discovery_complete() {
    DISCOVERY_DONE.store(true, Ordering::Release);
    #[cfg(test)]
    TEST_DISCOVERY_CLOCK.set((true, Duration::ZERO));
}

fn discovery_done() -> bool {
    let clock = (
        DISCOVERY_DONE.load(Ordering::Acquire),
        RECEIVER_STARTED.elapsed(),
    );
    #[cfg(test)]
    let clock = {
        let _ = clock;
        TEST_DISCOVERY_CLOCK.get()
    };
    if clock.0 {
        return true;
    }
    let expired = clock.1 >= DISCOVERY_GRACE;
    if expired && !GRACE_WARNED.swap(true, Ordering::AcqRel) {
        tracing::warn!("no rehydrate pass listed tmux in time; unmapped Claude hooks are accepted");
    }
    expired
}

#[cfg(test)]
pub(crate) fn set_discovery_pending_for_tests(pending: bool) {
    TEST_DISCOVERY_CLOCK.set((!pending, Duration::ZERO));
}

#[cfg(test)]
pub(crate) fn ingress_counters_for_tests() -> (u64, u64) {
    (
        UNMAPPED_COMMAND_SESSIONS.load(Ordering::Acquire),
        LEGACY_NOT_DURABLE.load(Ordering::Acquire),
    )
}

/// The one binding judgment of a hook; only `Unavailable` and `NotDurable` refuse it.
pub(crate) fn observe_binding_hook(
    provider: &str,
    event: &str,
    command_session_id: Option<&str>,
    payload_session_id: Option<&str>,
    payload: &Value,
    headers: &HeaderMap,
) -> IngressOutcome {
    let (Some(command), Some(payload_session)) = (command_session_id, payload_session_id) else {
        return IngressOutcome::Proceed(ProceedReason::NoSessionSwitch);
    };
    if command == payload_session
        && !(provider == "codex"
            && HookEventKind::from_path(event) == HookEventKind::SessionStart
            && payload["source"] == "clear")
    {
        return IngressOutcome::Proceed(ProceedReason::NoSessionSwitch);
    }
    let envelope = headers
        .get(BINDING_HEADER)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| decode_binding_header(h).ok());
    let hook = HookSignal::from_payload(HookEventKind::from_path(event).as_str(), payload);
    match provider {
        "claude" => {}
        "codex" => {
            return crate::services::tui_prompt_dedupe::observe_codex_hook(
                command,
                payload_session,
                &hook,
                envelope.as_ref(),
            );
        }
        _ => return IngressOutcome::NotApplicable(NotApplicableReason::OtherProvider),
    }
    if pane_registration_failed(command, envelope.as_ref()) {
        return IngressOutcome::Unavailable(UnavailableReason::PaneRegistrationFailed);
    }
    match adoption_retry::adopt_from_hook(command, payload_session, &hook) {
        AdoptionHttp::Durable(kind) => IngressOutcome::Durable(kind),
        AdoptionHttp::NotDurable(reason) => IngressOutcome::NotDurable(reason),
        AdoptionHttp::Skipped(skip) => classify_skip(skip, command, envelope.as_ref()),
    }
}

fn classify_skip(
    skip: AdoptSkip,
    command_session_id: &str,
    envelope: Option<&crate::services::tui_prompt_dedupe::binding_context::HookBindingEnvelope>,
) -> IngressOutcome {
    use IngressOutcome::{NotApplicable, Unavailable};
    match skip {
        // A pane whose launch binding failed stays refused even after discovery or its grace.
        AdoptSkip::UnmappedCommandSession
            if pane_registration_failed(command_session_id, envelope) =>
        {
            Unavailable(UnavailableReason::PaneRegistrationFailed)
        }
        AdoptSkip::UnmappedCommandSession if !discovery_done() => {
            Unavailable(UnavailableReason::RestoreNotReady)
        }
        AdoptSkip::UnmappedCommandSession => {
            UNMAPPED_COMMAND_SESSIONS.fetch_add(1, Ordering::AcqRel);
            NotApplicable(NotApplicableReason::UnmappedCommandSession)
        }
        AdoptSkip::NoChannel => IngressOutcome::Proceed(ProceedReason::NoChannelLog),
        AdoptSkip::MtimeUnreadable => Unavailable(UnavailableReason::MtimeUnreadable),
        AdoptSkip::ChannelNotRestored => Unavailable(UnavailableReason::ChannelNotRestored),
        AdoptSkip::PayloadNotUuid => NotApplicable(NotApplicableReason::PayloadNotUuid),
        AdoptSkip::NotClaudeTui => NotApplicable(NotApplicableReason::NotClaudeTui),
        AdoptSkip::MalformedBindingPath => NotApplicable(NotApplicableReason::MalformedBindingPath),
        AdoptSkip::OlderThanBound => NotApplicable(NotApplicableReason::OlderThanBound),
    }
}

/// 425 for a refused outcome; the in-flight receipt is dropped so the same request id retries.
pub(crate) fn refusal(
    ledger: &RelayReceiptLedger,
    ticket: Option<RelayReceiptTicket>,
    outcome: IngressOutcome,
    provider: &str,
    event: &str,
) -> Option<(StatusCode, Json<Value>)> {
    if !outcome.refused() {
        return None;
    }
    match ticket {
        Some(ticket) => ledger.abandon(ticket),
        None => {
            LEGACY_NOT_DURABLE.fetch_add(1, Ordering::AcqRel);
            tracing::error!(
                provider,
                event,
                ?outcome,
                "hook without a relay request id refused; nothing retries it automatically"
            );
        }
    }
    tracing::warn!(
        provider,
        event,
        ?outcome,
        "hook refused until its binding observation is durable"
    );
    let body = json!({
        "ok": false,
        "error": "binding observation is not durable yet",
        "reason": format!("{outcome:?}"),
    });
    Some((StatusCode::TOO_EARLY, Json(body)))
}

#[cfg(test)]
#[path = "observation_ingress_tests.rs"]
pub(crate) mod tests;
