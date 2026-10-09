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
use super::relay_receipts::{RELAY_PUBLISHED_AT_HEADER, RelayReceiptLedger, RelayReceiptTicket};
use crate::services::tui_prompt_dedupe::AdoptSkip;
use crate::services::tui_prompt_dedupe::binding_events::HookSignal;

/// Without a listed tmux, unmapped hooks of panes this runtime did not launch stop being refused
/// this long after the receiver starts.
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
    ResumeConflict,
    UnmappedCommandSession,
    PayloadPathMissing,
    SourceRejected,
    SourceAnomaly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UnavailableReason {
    RestoreNotReady,
    PaneRegistrationFailed,
    ChannelNotRestored,
    RuntimeNotRestored,
    HistoryUnreadable,
    SourceUnreadable,
    HostNotAdmitted,
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

/// Whether a rehydrate pass has listed tmux in this process; the grace expiry does not count.
pub(crate) fn boot_discovery_done() -> bool {
    DISCOVERY_DONE.load(Ordering::Acquire)
}

/// `(a rehydrate pass listed tmux, time since the receiver started)`.
fn discovery_clock() -> (bool, Duration) {
    let clock = (
        DISCOVERY_DONE.load(Ordering::Acquire),
        RECEIVER_STARTED.elapsed(),
    );
    #[cfg(test)]
    let clock = {
        let _ = clock;
        TEST_DISCOVERY_CLOCK.get()
    };
    clock
}

fn discovery_done() -> bool {
    let clock = discovery_clock();
    if clock.0 {
        return true;
    }
    let expired = clock.1 >= DISCOVERY_GRACE;
    if expired && !GRACE_WARNED.swap(true, Ordering::AcqRel) {
        tracing::warn!(
            "no rehydrate pass listed tmux in time; unmapped hooks of unlaunched panes are accepted"
        );
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
    let published_at = headers
        .get(RELAY_PUBLISHED_AT_HEADER)
        .and_then(|h| h.to_str().ok());
    let published_at = published_at.and_then(|h| chrono::DateTime::parse_from_rfc3339(h).ok());
    let hook = HookSignal {
        published_at: published_at.map(|t| t.with_timezone(&chrono::Utc)),
        ..HookSignal::from_payload(HookEventKind::from_path(event).as_str(), payload)
    };
    let envelope = headers
        .get(BINDING_HEADER)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| decode_binding_header(h).ok());
    if provider == "codex"
        && let Some(outcome) = crate::services::tui_prompt_dedupe::observe_verified_codex_hook(
            payload_session_id,
            payload,
            &hook,
            envelope.as_ref(),
        )
    {
        return outcome;
    }
    if provider == "codex"
        && crate::services::codex::codex_source_mode_snapshot()
            == crate::services::codex::CodexSourceMode::Shadow
    {
        crate::services::tui_prompt_dedupe::observe_codex_shadow(
            command_session_id,
            payload_session_id,
            payload,
            &hook,
            envelope.as_ref(),
        );
    }
    let (Some(command), Some(payload_session)) = (command_session_id, payload_session_id) else {
        return IngressOutcome::Proceed(ProceedReason::NoSessionSwitch);
    };
    if command == payload_session
        && !(provider == "codex"
            && HookEventKind::from_path(event) == HookEventKind::SessionStart
            && payload["source"] == "clear")
    {
        // A prompt of the launch session may still supersede a Pending it outlived; no switch.
        let prompt = HookEventKind::from_path(event) == HookEventKind::UserPromptSubmit;
        #[cfg(test)]
        let prompt = prompt
            && !crate::services::claude_tui::source_verify::n2b_mutant("r5-reclaim-ingress-off");
        if provider == "claude"
            && let Err(reason) =
                adoption_retry::observe_launch_hook(command, &hook, envelope.as_ref(), prompt)
        {
            return IngressOutcome::NotDurable(reason);
        }
        #[cfg(test)]
        if provider == "claude"
            && !prompt
            && crate::services::claude_tui::source_verify::n2b_mutant("r5-ingress-start")
        {
            return match adoption_retry::adopt_from_enveloped_hook(
                command,
                payload_session,
                &hook,
                envelope.as_ref(),
            ) {
                AdoptionHttp::Durable(kind) => IngressOutcome::Durable(kind),
                AdoptionHttp::NotDurable(reason) => IngressOutcome::NotDurable(reason),
                AdoptionHttp::Skipped(skip) => classify_skip(skip, command, None),
            };
        }
        return IngressOutcome::Proceed(ProceedReason::NoSessionSwitch);
    }
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
    #[cfg(test)]
    crate::services::tui_prompt_dedupe::before_authority(command);
    match adoption_retry::adopt_from_enveloped_hook(
        command,
        payload_session,
        &hook,
        envelope.as_ref(),
    ) {
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
        AdoptSkip::UnmappedCommandSession
            if !discovery_done() || (!discovery_clock().0 && managed_here(envelope)) =>
        {
            Unavailable(UnavailableReason::RestoreNotReady)
        }
        AdoptSkip::UnmappedCommandSession => {
            UNMAPPED_COMMAND_SESSIONS.fetch_add(1, Ordering::AcqRel);
            NotApplicable(NotApplicableReason::UnmappedCommandSession)
        }
        AdoptSkip::NoChannel => IngressOutcome::Proceed(ProceedReason::NoChannelLog),
        AdoptSkip::ChannelNotRestored => Unavailable(UnavailableReason::ChannelNotRestored),
        AdoptSkip::RuntimeNotRestored => Unavailable(UnavailableReason::RuntimeNotRestored),
        AdoptSkip::PayloadNotUuid => NotApplicable(NotApplicableReason::PayloadNotUuid),
        AdoptSkip::NotClaudeTui => NotApplicable(NotApplicableReason::NotClaudeTui),
        AdoptSkip::MalformedBindingPath => NotApplicable(NotApplicableReason::MalformedBindingPath),
        AdoptSkip::ResumeConflict => NotApplicable(NotApplicableReason::ResumeConflict),
        AdoptSkip::PayloadPathMissing => NotApplicable(NotApplicableReason::PayloadPathMissing),
        AdoptSkip::SourceRejected(_) => NotApplicable(NotApplicableReason::SourceRejected),
        AdoptSkip::SourceAnomaly => NotApplicable(NotApplicableReason::SourceAnomaly),
        AdoptSkip::HistoryUnreadable => Unavailable(UnavailableReason::HistoryUnreadable),
        AdoptSkip::SourceUnreadable => Unavailable(UnavailableReason::SourceUnreadable),
        AdoptSkip::HostNotAdmitted => Unavailable(UnavailableReason::HostNotAdmitted),
    }
}

/// A hook from a Claude pane this runtime launched; the grace expiry never gives up on it, only
/// a rehydrate pass that listed tmux does.
fn managed_here(
    envelope: Option<&crate::services::tui_prompt_dedupe::binding_context::HookBindingEnvelope>,
) -> bool {
    use crate::services::tui_prompt_dedupe::binding_context::CapturedContext;
    matches!(envelope.map(|e| &e.context), Some(CapturedContext::Captured(ctx))
        if ctx.schema == 1
            && ctx.provider == "claude"
            && ctx.owner_runtime_root == crate::services::tmux_common::current_tmux_owner_marker())
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

#[cfg(test)]
#[cfg(windows)]
mod windows_tests {
    use crate::services::claude_tui::source_verify::{
        ClaudeHookSource, SourceHistory, SourceRejection, SourceVerdict, observe_transcript,
        verify_claude_source,
    };
    use crate::services::codex_tui::session::source_observation::{
        CodexHookSourceClaim, CodexHookSourceRejection, CodexRolloutSource,
        verify_codex_hook_source,
    };

    #[test]
    fn a_claude_transcript_without_supported_file_identity_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let id = uuid::Uuid::new_v4().hyphenated().to_string();
        let path = project.join(format!("{id}.jsonl"));
        let first = serde_json::json!({"type": "mode", "sessionId": id});
        std::fs::write(&path, format!("{first}\n")).unwrap();
        let source = ClaudeHookSource {
            event: "session-start".into(),
            session_id: id.clone(),
            transcript_path: path.clone(),
            start_source: Some("startup".into()),
            published_at: None,
        };
        let opened = observe_transcript(&path).unwrap();
        assert_eq!(
            verify_claude_source(
                &source,
                root.path(),
                Ok(&opened),
                &id,
                None,
                &SourceHistory::default(),
            ),
            SourceVerdict::Rejected(SourceRejection::IdentityUnavailable)
        );
    }

    #[test]
    fn a_rollout_without_supported_file_identity_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4().hyphenated().to_string();
        let path = root.path().join(format!("rollout-{id}.jsonl"));
        let meta = serde_json::json!({
            "type": "session_meta",
            "payload": {"id": id, "cwd": root.path(), "source": "cli", "originator": "codex_cli_rs"}
        });
        std::fs::write(&path, format!("{meta}\n")).unwrap();
        let claim = CodexHookSourceClaim {
            session_id: &id,
            transcript_path: Some(&path),
            expected_source: CodexRolloutSource::Cli,
        };
        assert_eq!(
            verify_codex_hook_source(root.path(), &claim),
            Err(CodexHookSourceRejection::RolloutUnavailable)
        );
    }
}
