//! Host executor for Claude TUI input: the target, a gate checked before every
//! pane mutation, and typed outcomes. A confirmed tmux session takes keys; a recorded
//! Herdr pane takes them only through its own gate.

use std::process::Output;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NativeClearSubmission {
    NotSent,
    Confirmed,
    Indeterminate,
}

pub(crate) fn submit_native_clear_tmux(
    target: &InputTarget,
    gate: &dyn MutationGate,
    deadline: tokio::time::Instant,
) -> NativeClearSubmission {
    #[cfg(not(unix))]
    {
        let _ = (target, gate, deadline);
        return NativeClearSubmission::NotSent;
    }
    // The caller supplies the resolved target; only a confirmed tmux session takes keys.
    let InputTarget::Tmux(session) = target else {
        return NativeClearSubmission::NotSent;
    };
    super::composer_lock::try_with_composer_mutation_lock(&session, || {
        native_clear_once(
            &session,
            gate,
            deadline,
            |remaining| tmux::capture_pane_timeout(&session, -80, remaining),
            |remaining| capture_draft_with("tmux", &session, remaining),
            |keys, remaining| {
                tmux::send_keys_timeout(&session, keys, remaining).is_ok_and(|o| o.status.success())
            },
        )
    })
    .unwrap_or(NativeClearSubmission::NotSent)
}

pub(super) fn native_clear_once(
    session: &str,
    gate: &dyn MutationGate,
    deadline: tokio::time::Instant,
    mut capture: impl FnMut(std::time::Duration) -> Option<String>,
    draft: impl FnOnce(std::time::Duration) -> Option<String>,
    mut send: impl FnMut(&[&str], std::time::Duration) -> bool,
) -> NativeClearSubmission {
    // The shared subprocess owner may spend 200ms killing and 2s reaping a timed-out child.
    let remaining = || {
        deadline
            .saturating_duration_since(tokio::time::Instant::now())
            .saturating_sub(std::time::Duration::from_millis(2300))
    };
    let budget = remaining();
    if budget.is_zero() {
        return NativeClearSubmission::NotSent;
    }
    let Some(before) = capture(budget) else {
        return NativeClearSubmission::NotSent;
    };
    if !native_clear_composer_empty(&before)
        || remaining().is_zero()
        || super::composer_lock::admit_composer_write(session, || draft(remaining())).is_err()
        || gate.admit(session).is_err()
    {
        return NativeClearSubmission::NotSent;
    }
    crate::services::tui_prompt_dedupe::record_discord_originated_prompt(
        "claude", session, "/clear",
    );
    // One command submits only slash-command characters and Enter; no retry follows.
    let sent = send(&["/clear", "Enter"], remaining());
    if !sent {
        return NativeClearSubmission::Indeterminate;
    }
    if !remaining().is_zero()
        && capture(remaining()).is_some_and(|c| native_clear_composer_empty(&c))
    {
        NativeClearSubmission::Confirmed
    } else {
        NativeClearSubmission::Indeterminate
    }
}

pub(crate) fn native_clear_composer_empty(capture: &str) -> bool {
    use crate::services::tmux_common as tc;
    tc::tmux_capture_indicates_claude_tui_exact_empty_composer(capture)
        && !tc::tmux_capture_indicates_claude_tui_busy(capture)
        && !tc::tmux_capture_indicates_claude_tui_interactive_modal(capture)
}

use super::input::{
    POST_LITERAL_SETTLE, POST_PASTE_BUFFER_SETTLE, PROMPT_READY_CANCELLED_ERROR,
    PromptReadinessKind, TuiInputAction, ensure_tmux_success, literal_action_needs_post_settle,
    prompt_marker_confirms_prompt_ready, prompt_readiness_snapshot_from_capture,
};
use super::startup_dialog::detect_claude_startup_dialog;
use crate::db::dispatched_sessions::hosted_execution::HostedExecution;
use crate::services::platform::tmux;
use crate::services::provider::session_probe::SessionLiveness;
use crate::services::provider::{CancelToken, cancel_requested};
use crate::services::session_host::{
    HerdrGateRefusal, HerdrMutation, HerdrTarget, HostError, HostKey, HostKind, HostMutation,
    ResolvedSessionTarget, TargetHost, TmuxHost, herdr_endpoints,
};
use crate::services::tmux_common::tmux_capture_indicates_claude_tui_interactive_modal;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InputTarget {
    /// A confirmed tmux session.
    Tmux(String),
    /// A recorded Herdr pane; it takes keys only through its own gate.
    Herdr(HerdrTarget),
    Refused(InputRefusal),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputRefusal {
    Unsupported(HostKind),
    Unknown,
    Conflict,
    IdentityMismatch,
    Composer(crate::services::tui_input::submission::Refusal),
    Herdr(HerdrGateRefusal),
}

impl InputTarget {
    /// Production entry, decided before any IO: no Herdr selector exists, so a
    /// Claude TUI session is the legacy tmux session of that name.
    pub(crate) fn legacy_tmux(session_name: &str) -> Self {
        Self::Tmux(session_name.to_string())
    }

    /// Resolved host evidence; Unknown and Conflict never become tmux. Without its stored
    /// execution a Herdr pane has no gate, so it is refused.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn from_session_target(target: &ResolvedSessionTarget) -> Self {
        Self::from_host(&target.host, |_| {
            Self::Refused(InputRefusal::Unsupported(HostKind::Herdr))
        })
    }

    /// Resolved evidence with the row's stored execution: a Herdr pane becomes a target only
    /// on a registered local endpoint that holds that execution, before any IO.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn from_hosted_target(
        target: &ResolvedSessionTarget,
        stored: &HostedExecution,
    ) -> Self {
        Self::from_host(&target.host, |pane| {
            match herdr_endpoints().target(stored) {
                Some(herdr) if herdr.pane_id() == pane => Self::Herdr(herdr),
                Some(_) => Self::Refused(InputRefusal::Conflict),
                None => Self::Refused(InputRefusal::Unsupported(HostKind::Herdr)),
            }
        })
    }

    fn from_host(host: &TargetHost, herdr: impl FnOnce(&str) -> Self) -> Self {
        match host {
            TargetHost::Known {
                kind: HostKind::Tmux,
                name,
                ..
            } => Self::Tmux(name.clone()),
            TargetHost::Known {
                kind: HostKind::Herdr,
                name,
                ..
            } => herdr(name),
            TargetHost::Known { kind, .. } => Self::Refused(InputRefusal::Unsupported(*kind)),
            TargetHost::Unknown(_) => Self::Refused(InputRefusal::Unknown),
            TargetHost::Conflict { .. } => Self::Refused(InputRefusal::Conflict),
        }
    }

    /// The session a tmux-only path may drive; a Herdr pane is not one.
    pub(crate) fn tmux_session(&self) -> Result<&str, InputRefusal> {
        match self {
            Self::Tmux(session) => Ok(session),
            Self::Herdr(_) => Err(InputRefusal::Unsupported(HostKind::Herdr)),
            Self::Refused(refusal) => Err(*refusal),
        }
    }

    /// Legacy keys may follow a stopped plan only on a confirmed tmux target
    /// that no gate refused; anywhere else nothing more is sent.
    pub(crate) fn keys_may_follow(&self, run: &InputRun) -> bool {
        matches!(self, Self::Tmux(_))
            && !matches!(
                run,
                InputRun::Refused(_)
                    | InputRun::Indeterminate {
                        cause: StopCause::Refused(_),
                        ..
                    }
            )
    }
}

/// Checked before every pane mutation; the execution validator implements it.
pub(crate) trait MutationGate {
    fn admit(&self, session: &str) -> Result<(), InputRefusal>;
    /// A cancel key only stops work; a gate may admit it where it admits no other input.
    fn admit_cancel(&self, session: &str) -> Result<(), InputRefusal> {
        self.admit(session)
    }
}

/// Herdr's gate judges each write and pins the judgment for the send right after it.
impl MutationGate for HerdrTarget {
    fn admit(&self, session: &str) -> Result<(), InputRefusal> {
        self.admit_as(session, HerdrMutation::Input)
    }

    fn admit_cancel(&self, session: &str) -> Result<(), InputRefusal> {
        self.admit_as(session, HerdrMutation::Cancel)
    }
}

impl HerdrTarget {
    fn admit_as(&self, session: &str, mutation: HerdrMutation) -> Result<(), InputRefusal> {
        if session != self.pane_id() {
            self.discard_pin();
            return Err(InputRefusal::Conflict);
        }
        self.pin(mutation).map_err(InputRefusal::Herdr)
    }
}

/// Legacy tmux keeps no stored execution evidence to re-check, so it admits.
pub(crate) struct LegacyTmuxGate;

impl MutationGate for LegacyTmuxGate {
    fn admit(&self, _session: &str) -> Result<(), InputRefusal> {
        Ok(())
    }
}

/// Pane operations input needs; tmux is the only production transport.
pub(crate) trait InputTransport {
    fn send_literal(&mut self, session: &str, text: &str) -> Result<Output, String>;
    fn load_buffer(&mut self, buffer: &str, text: &str) -> Result<Output, String>;
    fn paste_buffer(&mut self, session: &str, buffer: &str, delete: bool)
    -> Result<Output, String>;
    fn send_keys(&mut self, session: &str, keys: &[HostKey]) -> Result<Output, String>;
    fn capture(&mut self, session: &str, scroll_back: i32) -> Option<String>;
    /// The pane for the draft-protection reader; a transport that cannot keep its attributes
    /// returns nothing, which keeps a protected pane held.
    fn capture_draft(&mut self, _session: &str) -> Option<String> {
        None
    }
    fn pane_alive(&mut self, session: &str) -> bool;
    fn present(&mut self, session: &str) -> bool;
    /// Records the termination and exit reason, then kills the session.
    fn retire(&mut self, session: &str, reason_code: &str, reason: &str);
}

struct TmuxInput;

impl InputTransport for TmuxInput {
    fn send_literal(&mut self, session: &str, text: &str) -> Result<Output, String> {
        tmux::send_literal(session, text)
    }

    fn load_buffer(&mut self, buffer: &str, text: &str) -> Result<Output, String> {
        tmux::load_buffer(buffer, text)
    }

    fn paste_buffer(
        &mut self,
        session: &str,
        buffer: &str,
        delete: bool,
    ) -> Result<Output, String> {
        tmux::paste_buffer(session, buffer, delete)
    }

    fn send_keys(&mut self, session: &str, keys: &[HostKey]) -> Result<Output, String> {
        TmuxHost.send_host_keys(session, keys)
    }

    fn capture(&mut self, session: &str, scroll_back: i32) -> Option<String> {
        tmux::capture_pane(session, scroll_back)
    }

    fn capture_draft(&mut self, session: &str) -> Option<String> {
        #[cfg(test)]
        if let Some(program) = spy::draft_route(session) {
            return capture_draft_with(program, session, DRAFT_CAPTURE_TIMEOUT);
        }
        capture_draft_with("tmux", session, DRAFT_CAPTURE_TIMEOUT)
    }

    fn pane_alive(&mut self, session: &str) -> bool {
        crate::services::tmux_diagnostics::tmux_session_has_live_pane(session)
    }

    fn present(&mut self, session: &str) -> bool {
        tmux::has_session(session)
    }

    fn retire(&mut self, session: &str, reason_code: &str, reason: &str) {
        crate::services::termination_audit::record_termination_for_tmux(
            session,
            None,
            "claude_tui_provider",
            reason_code,
            Some(reason),
            None,
        );
        crate::services::tmux_diagnostics::record_tmux_exit_reason(session, reason);
        tmux::kill_session(session, reason);
    }
}

/// Input to a Herdr pane: each write goes out under the judgment its gate pinned just
/// before it, and retire kills nothing.
pub(crate) struct HerdrInput<'a> {
    target: &'a HerdrTarget,
    buffers: std::collections::HashMap<String, String>,
}

impl<'a> HerdrInput<'a> {
    pub(crate) fn new(target: &'a HerdrTarget) -> Self {
        Self {
            target,
            buffers: Default::default(),
        }
    }
}

/// A Herdr write as the executor's process-shaped result: only a confirmed write succeeds.
fn herdr_written(result: Result<HostMutation, HostError>) -> Result<Output, String> {
    match result {
        Ok(HostMutation::Confirmed) => Ok(Output {
            status: Default::default(),
            stdout: Vec::new(),
            stderr: Vec::new(),
        }),
        Ok(HostMutation::Refused(refusal)) => Err(format!("herdr input refused: {refusal:?}")),
        Ok(HostMutation::Indeterminate(detail)) => {
            Err(format!("herdr input may have landed: {detail}"))
        }
        Err(error) => Err(format!("herdr input not sent: {error:?}")),
    }
}

impl InputTransport for HerdrInput<'_> {
    fn send_literal(&mut self, _session: &str, text: &str) -> Result<Output, String> {
        herdr_written(self.target.send_text(text))
    }

    /// Kept here until its paste, which is judged again; nothing reaches the pane.
    fn load_buffer(&mut self, buffer: &str, text: &str) -> Result<Output, String> {
        self.target.discard_pin();
        self.buffers.insert(buffer.to_string(), text.to_string());
        herdr_written(Ok(HostMutation::Confirmed))
    }

    fn paste_buffer(&mut self, _s: &str, buffer: &str, _delete: bool) -> Result<Output, String> {
        let text = self.buffers.remove(buffer);
        let text = text.ok_or_else(|| format!("herdr paste of unknown buffer {buffer}"))?;
        herdr_written(self.target.send_paste(&text))
    }

    fn send_keys(&mut self, _session: &str, keys: &[HostKey]) -> Result<Output, String> {
        herdr_written(self.target.send_keys(keys))
    }

    fn capture(&mut self, _session: &str, scroll_back: i32) -> Option<String> {
        self.target.capture(scroll_back)
    }

    fn pane_alive(&mut self, _session: &str) -> bool {
        self.target.execution_alive()
    }

    fn present(&mut self, _session: &str) -> bool {
        self.target.present()
    }

    /// Herdr has no kill here; the pane and its execution stay as they are.
    fn retire(&mut self, session: &str, reason_code: &str, _reason: &str) {
        tracing::warn!(pane = session, reason_code, "herdr pane retire refused");
    }
}

#[cfg(test)]
thread_local! {
    static INJECTED: std::cell::RefCell<Option<Box<dyn InputTransport>>> =
        const { std::cell::RefCell::new(None) };
}

fn with_transport<R>(operation: impl FnOnce(&mut dyn InputTransport) -> R) -> R {
    with_transport_using(&mut TmuxInput, operation)
}

fn with_transport_using<R>(
    default: &mut dyn InputTransport,
    operation: impl FnOnce(&mut dyn InputTransport) -> R,
) -> R {
    #[cfg(test)]
    if let Some(mut injected) = INJECTED.with(|slot| slot.borrow_mut().take()) {
        let result = operation(injected.as_mut());
        INJECTED.with(|slot| *slot.borrow_mut() = Some(injected));
        return result;
    }
    operation(default)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InputRun {
    Applied,
    /// Nothing reached the pane.
    Refused(InputRefusal),
    Cancelled {
        confirmed: usize,
    },
    /// A send began and may have landed: later steps, Enter and retries stay unsent.
    Indeterminate {
        confirmed: usize,
        cause: StopCause,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StopCause {
    Refused(InputRefusal),
    Send(String),
}

impl InputRun {
    /// The `Result` legacy tmux callers return, with their original error text.
    pub(crate) fn into_legacy(self) -> Result<(), String> {
        match self {
            Self::Applied => Ok(()),
            Self::Cancelled { .. } => Err(PROMPT_READY_CANCELLED_ERROR.to_string()),
            Self::Indeterminate {
                cause: StopCause::Send(error),
                ..
            } => Err(error),
            Self::Refused(refusal) => Err(refused_message(refusal)),
            Self::Indeterminate {
                cause: StopCause::Refused(refusal),
                ..
            } => Err(format!("claude tui input held after mutation: {refusal:?}")),
        }
    }
}

fn refused_message(refusal: InputRefusal) -> String {
    format!("claude tui input refused before mutation: {refusal:?}")
}

/// Runs `actions` on `target`, admitting every pane mutation through `gate`.
pub(crate) fn run_plan(
    target: &InputTarget,
    gate: &dyn MutationGate,
    transport: &mut dyn InputTransport,
    actions: &[TuiInputAction],
    cancel_token: Option<&CancelToken>,
) -> InputRun {
    let session = match target {
        InputTarget::Tmux(session) => session.as_str(),
        InputTarget::Herdr(herdr) => herdr.pane_id(),
        InputTarget::Refused(refusal) => return InputRun::Refused(*refusal),
    };
    let mut plan = Plan {
        session,
        gate,
        transport,
        cancel_token,
        confirmed: 0,
    };
    for (index, action) in actions.iter().enumerate() {
        if let Err(stopped) = plan.step(action, actions.get(index + 1)) {
            return stopped;
        }
    }
    InputRun::Applied
}

struct Plan<'a> {
    session: &'a str,
    gate: &'a dyn MutationGate,
    transport: &'a mut dyn InputTransport,
    cancel_token: Option<&'a CancelToken>,
    confirmed: usize,
}

type SendOp<'s> = Box<dyn FnOnce(&mut dyn InputTransport) -> Result<Output, String> + 's>;

impl Plan<'_> {
    fn step(
        &mut self,
        action: &TuiInputAction,
        next: Option<&TuiInputAction>,
    ) -> Result<(), InputRun> {
        self.check_cancel()?;
        let session = self.session;
        let key = |key: HostKey| -> SendOp<'_> { Box::new(move |t| t.send_keys(session, &[key])) };
        match action {
            TuiInputAction::Literal(text) => {
                self.mutate(action, Box::new(|t| t.send_literal(session, text)))?;
            }
            TuiInputAction::PasteBuffer(text) => {
                let buffer = format!("agentdesk-tui-input-{}", uuid::Uuid::new_v4());
                self.admit(false)?;
                self.send(action, Box::new(|t| t.load_buffer(&buffer, text)))?;
                self.check_cancel()?;
                self.mutate(action, Box::new(|t| t.paste_buffer(session, &buffer, true)))?;
                std::thread::sleep(POST_PASTE_BUFFER_SETTLE);
                self.check_cancel()?;
            }
            TuiInputAction::Enter => self.mutate(action, key(HostKey::Enter))?,
            TuiInputAction::Escape => self.mutate(action, key(HostKey::Escape))?,
            TuiInputAction::CtrlU => self.mutate(action, key(HostKey::CtrlU))?,
            TuiInputAction::ArrowLeft => self.mutate(action, key(HostKey::Left))?,
            TuiInputAction::ArrowRight => self.mutate(action, key(HostKey::Right))?,
            TuiInputAction::Backspace(count) => {
                let mut remaining = *count;
                while remaining > 0 {
                    let batch = vec![HostKey::Backspace; remaining.min(32)];
                    remaining -= batch.len();
                    self.mutate(action, Box::new(move |t| t.send_keys(session, &batch)))?;
                }
            }
        }
        if literal_action_needs_post_settle(action, next) {
            self.check_cancel()?;
            std::thread::sleep(POST_LITERAL_SETTLE);
        }
        Ok(())
    }

    fn check_cancel(&self) -> Result<(), InputRun> {
        if cancel_requested(self.cancel_token) {
            return Err(InputRun::Cancelled {
                confirmed: self.confirmed,
            });
        }
        Ok(())
    }

    fn admit(&self, cancel: bool) -> Result<(), InputRun> {
        admit_after(self.gate, self.session, self.confirmed, cancel)
    }

    fn send(&mut self, action: &TuiInputAction, send: SendOp<'_>) -> Result<(), InputRun> {
        send(&mut *self.transport)
            .and_then(|output| ensure_tmux_success(output, action))
            .map_err(|error| InputRun::Indeterminate {
                confirmed: self.confirmed,
                cause: StopCause::Send(error),
            })
    }

    fn mutate(&mut self, action: &TuiInputAction, send: SendOp<'_>) -> Result<(), InputRun> {
        self.admit(matches!(action, TuiInputAction::Escape))?;
        self.send(action, send)?;
        self.confirmed += 1;
        Ok(())
    }
}

fn admit_after(
    gate: &dyn MutationGate,
    session: &str,
    confirmed: usize,
    cancel: bool,
) -> Result<(), InputRun> {
    let admitted = if cancel {
        gate.admit_cancel(session)
    } else {
        gate.admit(session)
    };
    admitted.map_err(|refusal| match confirmed {
        0 => InputRun::Refused(refusal),
        confirmed => InputRun::Indeterminate {
            confirmed,
            cause: StopCause::Refused(refusal),
        },
    })
}

/// Gated multi-key sends: each group is one send and keeps its legacy error name.
#[cfg(unix)]
pub(crate) struct KeyGroups<'a> {
    pub(crate) session: &'a str,
    gate: &'a dyn MutationGate,
    cancel_token: Option<&'a CancelToken>,
    confirmed: usize,
}

#[cfg(unix)]
impl<'a> KeyGroups<'a> {
    pub(crate) fn new(
        session: &'a str,
        gate: &'a dyn MutationGate,
        cancel_token: Option<&'a CancelToken>,
    ) -> Self {
        Self {
            session,
            gate,
            cancel_token,
            confirmed: 0,
        }
    }

    pub(crate) fn check_cancel(&self) -> Result<(), InputRun> {
        if cancel_requested(self.cancel_token) {
            return Err(InputRun::Cancelled {
                confirmed: self.confirmed,
            });
        }
        Ok(())
    }

    pub(crate) fn send(&mut self, keys: &[HostKey], name: &str) -> Result<(), InputRun> {
        let cancel = !keys.is_empty() && keys.iter().all(|key| *key == HostKey::Escape);
        admit_after(self.gate, self.session, self.confirmed, cancel)?;
        let session = self.session;
        with_transport(|transport| transport.send_keys(session, keys))
            .and_then(|output| ensure_named_success(output, name))
            .map_err(|error| InputRun::Indeterminate {
                confirmed: self.confirmed,
                cause: StopCause::Send(error),
            })?;
        self.confirmed += 1;
        Ok(())
    }
}

#[cfg(unix)]
fn ensure_named_success(output: Output, name: &str) -> Result<(), String> {
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if stderr.is_empty() {
        Err(format!("tmux send {name} failed: {}", output.status))
    } else {
        Err(format!("tmux send {name} failed: {stderr}"))
    }
}

/// A plan on a Herdr pane: its own gate admits each write and pins the server for it.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn run_herdr(
    target: &HerdrTarget,
    actions: &[TuiInputAction],
    cancel_token: Option<&CancelToken>,
) -> InputRun {
    let mut input = HerdrInput::new(target);
    let plan_target = InputTarget::Herdr(target.clone());
    run_plan(&plan_target, target, &mut input, actions, cancel_token)
}

/// A plan on the legacy tmux session of `session_name`.
pub(crate) fn run_legacy(
    session_name: &str,
    actions: &[TuiInputAction],
    cancel_token: Option<&CancelToken>,
) -> InputRun {
    let target = InputTarget::legacy_tmux(session_name);
    with_transport(|transport| run_plan(&target, &LegacyTmuxGate, transport, actions, cancel_token))
}

/// Submits only a prompt plan; the caller holds the existing composer mutex throughout.
pub(crate) fn run_prompt_submission_legacy(
    session_name: &str,
    actions: &[TuiInputAction],
    cancel_token: Option<&CancelToken>,
) -> InputRun {
    use crate::services::tui_input::submission::{
        Refusal, Submission, run_prompt_submission_using,
    };
    use crate::services::tui_o::shadow::ShadowProvider;
    let Some((TuiInputAction::Enter, payload)) = actions.split_last() else {
        return InputRun::Refused(InputRefusal::Composer(Refusal::InvalidPrompt));
    };
    let mut frame = String::new();
    for action in payload {
        match action {
            TuiInputAction::Literal(text) | TuiInputAction::PasteBuffer(text) => {
                frame.push_str(text)
            }
            _ => return InputRun::Refused(InputRefusal::Composer(Refusal::InvalidPrompt)),
        }
    }
    with_transport_using(
        &mut crate::services::tui_input::submission_tmux::SubmissionTmux,
        |transport| {
            let before = transport
                .capture_draft(session_name)
                .filter(|_| transport.pane_alive(session_name));
            let transport = std::cell::RefCell::new(transport);
            let target = InputTarget::legacy_tmux(session_name);
            run_prompt_submission_using(
                Submission {
                    provider: ShadowProvider::Claude,
                    frame: &frame,
                    before: before.as_deref(),
                    mutations: payload.len(),
                },
                cancel_token,
                || {
                    run_plan(
                        &target,
                        &LegacyTmuxGate,
                        &mut **transport.borrow_mut(),
                        payload,
                        cancel_token,
                    )
                },
                || {
                    if matches!(payload.last(), Some(TuiInputAction::Literal(_))) {
                        std::thread::sleep(POST_LITERAL_SETTLE);
                    }
                    let mut transport = transport.borrow_mut();
                    transport
                        .capture_draft(session_name)
                        .filter(|_| transport.pane_alive(session_name))
                },
                || {
                    run_plan(
                        &target,
                        &LegacyTmuxGate,
                        &mut **transport.borrow_mut(),
                        &[TuiInputAction::Enter],
                        cancel_token,
                    )
                },
            )
        },
    )
}

/// Observes a guarded submit once, leaving every stranded draft untouched.
pub(crate) fn confirm_prompt_submission_passively(
    session_name: &str,
    cancel_token: Option<&CancelToken>,
) -> Result<(), String> {
    if cancel_requested(cancel_token) {
        return Err(PROMPT_READY_CANCELLED_ERROR.to_string());
    }
    let capture = with_transport_using(
        &mut crate::services::tui_input::submission_tmux::SubmissionTmux,
        |transport| {
            transport
                .capture_draft(session_name)
                .filter(|_| transport.pane_alive(session_name))
        },
    );
    if cancel_requested(cancel_token) {
        return Err(PROMPT_READY_CANCELLED_ERROR.to_string());
    }
    let Some(capture) = capture else {
        return Err("claude tui input held after mutation: confirmation unavailable".into());
    };
    let verdict = crate::services::tui_input::actor::gate::judge_pane(
        crate::services::tui_o::shadow::ShadowProvider::Claude,
        &capture,
    );
    let plain = crate::services::codex_tui::input::strip_ansi_escape_sequences(&capture);
    if verdict == crate::services::tui_input::actor::gate::PaneVerdict::Modal
        || !crate::services::tui_input::actor::gate::claude_composer_empty_when_present(&capture)
        || crate::services::tmux_common::tmux_capture_indicates_claude_tui_prompt_draft(&plain)
        || !(verdict == crate::services::tui_input::actor::gate::PaneVerdict::Ready
            || crate::services::tmux_common::tmux_capture_indicates_claude_tui_busy(&plain))
    {
        return Err("claude tui input held after mutation: confirmation ambiguous".into());
    }
    Ok(())
}

/// Bounds the draft-protection capture of a pane.
const DRAFT_CAPTURE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The last 80 rows with `-e`, so a faint placeholder stays apart from typed text; `program` is
/// tmux outside tests.
pub(crate) fn capture_draft_with(
    program: impl AsRef<std::ffi::OsStr>,
    session: &str,
    timeout: std::time::Duration,
) -> Option<String> {
    use crate::services::process::{configure_child_process_group, wait_with_output_timeout};
    let mut command = crate::services::platform::binary_resolver::runtime_command(program).ok()?;
    let args = ["-u", "capture-pane", "-p", "-e", "-t", session, "-S", "-80"];
    command.args(args).stdin(std::process::Stdio::null());
    command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    configure_child_process_group(&mut command);
    let child = command.spawn().ok()?;
    let output = wait_with_output_timeout(child, timeout, "tmux capture-pane -e").ok()?;
    let text = || String::from_utf8_lossy(&output.stdout).into_owned();
    output.status.success().then(text)
}

/// The legacy tmux session as the draft-protection reader needs it.
pub(crate) fn observe_draft(session_name: &str) -> Option<String> {
    with_transport(|transport| transport.capture_draft(session_name))
}

/// Capture, then liveness, of the legacy tmux session: the order input reads them.
pub(crate) fn observe_legacy(session_name: &str, scroll_back: i32) -> (Option<String>, bool) {
    with_transport(|transport| {
        let capture = transport.capture(session_name, scroll_back);
        (capture, transport.pane_alive(session_name))
    })
}

pub(crate) fn legacy_pane_alive(session_name: &str) -> bool {
    with_transport(|transport| transport.pane_alive(session_name))
}

pub(crate) fn legacy_present(session_name: &str) -> bool {
    with_transport(|transport| transport.present(session_name))
}

#[cfg(unix)]
pub(crate) fn legacy_retire(session_name: &str, reason_code: &str, reason: &str) {
    with_transport(|transport| transport.retire(session_name, reason_code, reason));
}

pub(crate) fn legacy_load_buffer(buffer: &str, text: &str) -> Result<Output, String> {
    with_transport(|transport| transport.load_buffer(buffer, text))
}

/// One pane write on the legacy tmux session, admitted by the gate first.
pub(crate) fn legacy_write(
    session_name: &str,
    write: impl FnOnce(&mut dyn InputTransport) -> Result<Output, String>,
) -> Result<Output, String> {
    LegacyTmuxGate
        .admit(session_name)
        .map_err(refused_message)?;
    with_transport(write)
}

/// A pane read; `Truncated` is a host saying it cut the text.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HostCapture {
    Complete(String),
    Truncated,
    Unavailable,
}

/// Executor result a follow-up consumes; never folded into a generic error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HostInputOutcome {
    Cleared,
    ReadySameExecution,
    Busy,
    PersistentDraft,
    UnknownTranscript,
    Refused(InputRefusal),
    Indeterminate {
        confirmed: usize,
    },
    Cancelled,
    /// Only a confirmed tmux session that reads dead.
    LegacyRecreateEligible,
}

#[cfg_attr(not(test), allow(dead_code))]
impl HostInputOutcome {
    pub(crate) fn allows_submit(&self) -> bool {
        matches!(self, Self::Cleared | Self::ReadySameExecution)
    }

    /// Only this hands recreation to the existing launch owner.
    pub(crate) fn allows_recreate(&self) -> bool {
        matches!(self, Self::LegacyRecreateEligible)
    }

    /// The outcome of a plan that stopped before applying every step.
    pub(crate) fn from_stopped_run(run: &InputRun) -> Option<Self> {
        match run {
            InputRun::Applied => None,
            InputRun::Refused(refusal) => Some(Self::Refused(*refusal)),
            InputRun::Cancelled { .. } => Some(Self::Cancelled),
            InputRun::Indeterminate { confirmed, .. } => Some(Self::Indeterminate {
                confirmed: *confirmed,
            }),
        }
    }

    /// After a clear plan: a stopped plan keeps its outcome; ready reads cleared.
    pub(crate) fn after_clear(run: &InputRun, observed: Self) -> Self {
        Self::from_stopped_run(run).unwrap_or(match observed {
            Self::ReadySameExecution => Self::Cleared,
            other => other,
        })
    }
}

/// Readiness from one observation. A blind, truncated or failed read is never
/// ready, and only a confirmed tmux session that reads dead may be recreated.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn classify(
    target: &InputTarget,
    liveness: SessionLiveness,
    capture: &HostCapture,
) -> HostInputOutcome {
    if let InputTarget::Refused(refusal) = target {
        return HostInputOutcome::Refused(*refusal);
    }
    match liveness {
        SessionLiveness::Alive => {}
        SessionLiveness::Missing if matches!(target, InputTarget::Tmux(_)) => {
            return HostInputOutcome::LegacyRecreateEligible;
        }
        SessionLiveness::Missing => {
            return HostInputOutcome::Refused(InputRefusal::Herdr(
                HerdrGateRefusal::PaneUnverified,
            ));
        }
        SessionLiveness::Unknown | SessionLiveness::ProbeFailed => {
            return HostInputOutcome::Refused(InputRefusal::Unknown);
        }
    }
    let HostCapture::Complete(text) = capture else {
        return HostInputOutcome::UnknownTranscript;
    };
    let snapshot = prompt_readiness_snapshot_from_capture(Some(text), true);
    // A mounted dialog takes Enter as its answer, whatever else the pane shows.
    if tmux_capture_indicates_claude_tui_interactive_modal(&snapshot.pane_tail)
        || detect_claude_startup_dialog(&snapshot.pane_tail).is_some()
    {
        HostInputOutcome::Busy
    } else if snapshot.prompt_draft_detected {
        HostInputOutcome::PersistentDraft
    } else if prompt_marker_confirms_prompt_ready(PromptReadinessKind::Followup, &snapshot) {
        HostInputOutcome::ReadySameExecution
    } else {
        HostInputOutcome::Busy
    }
}

#[cfg(all(test, unix))]
pub(crate) use spy::FakeDraftPane;
#[cfg(test)]
pub(crate) use spy::{SpyGuard, SpyState};

/// Recording transport the Claude TUI input tests install in place of tmux.
#[cfg(test)]
mod spy {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;

    use super::*;
    use crate::services::session_host::tmux_key_name;

    #[derive(Default)]
    pub(crate) struct SpyState {
        pub calls: Vec<String>,
        /// Answer for the n-th send (load, literal, paste or keys), counted from 0.
        pub fail_send: Option<(usize, Result<Output, String>)>,
        pub captures: VecDeque<Option<String>>,
        /// Captures after the first payload send, independent of readiness's read count.
        pub captures_after_send: Option<VecDeque<Option<String>>>,
        pub draft_capture_at: Option<(usize, Option<String>)>,
        pub dead: bool,
        pub absent: bool,
        pub sends: usize,
        /// Cancels this token when the n-th (from 1) call with this prefix is recorded.
        pub cancel_on: Option<(&'static str, usize, std::sync::Arc<CancelToken>)>,
    }

    pub(crate) struct Spy(pub Rc<RefCell<SpyState>>);

    impl SpyState {
        fn capture(&mut self) -> Option<String> {
            if self.sends > 0
                && let Some(after) = &mut self.captures_after_send
            {
                return after.pop_front().flatten();
            }
            self.captures.pop_front().flatten()
        }

        fn record(&mut self, call: String) {
            self.calls.push(call);
            if let Some((prefix, nth, token)) = &self.cancel_on
                && self.calls.iter().filter(|c| c.starts_with(prefix)).count() == *nth
                && self.calls.last().is_some_and(|c| c.starts_with(prefix))
            {
                token
                    .cancelled
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }

    impl Spy {
        fn send(&mut self, call: String) -> Result<Output, String> {
            let mut state = self.0.borrow_mut();
            state.record(call);
            let index = state.sends;
            state.sends += 1;
            match state.fail_send.take() {
                Some((at, answer)) if at == index => answer,
                other => {
                    state.fail_send = other;
                    Ok(exit(0, ""))
                }
            }
        }
    }

    fn without_escapes(capture: &str) -> String {
        let (mut out, mut chars) = (String::new(), capture.chars());
        while let Some(c) = chars.next() {
            if c != '\x1b' {
                out.push(c);
            } else if chars.next() == Some('[') {
                chars.by_ref().find(|c| ('\x40'..='\x7e').contains(c));
            }
        }
        out
    }

    pub(crate) fn exit(code: i32, stderr: &str) -> Output {
        #[cfg(unix)]
        let status = std::os::unix::process::ExitStatusExt::from_raw(code << 8);
        #[cfg(windows)]
        let status = std::os::windows::process::ExitStatusExt::from_raw(code as u32);
        Output {
            status,
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    impl InputTransport for Spy {
        fn send_literal(&mut self, _session: &str, text: &str) -> Result<Output, String> {
            self.send(format!("literal:{text}"))
        }

        fn load_buffer(&mut self, _buffer: &str, text: &str) -> Result<Output, String> {
            self.send(format!("load:{text}"))
        }

        fn paste_buffer(&mut self, _s: &str, _b: &str, delete: bool) -> Result<Output, String> {
            self.send(format!("paste:delete={delete}"))
        }

        fn send_keys(&mut self, _session: &str, keys: &[HostKey]) -> Result<Output, String> {
            let names: Vec<&str> = keys.iter().map(|key| tmux_key_name(*key)).collect();
            self.send(format!("keys:{}", names.join("+")))
        }

        /// Like tmux without `-e`: attributes are dropped.
        fn capture(&mut self, _session: &str, _scroll_back: i32) -> Option<String> {
            let mut state = self.0.borrow_mut();
            state.record("capture".to_string());
            state.capture().map(|c| without_escapes(&c))
        }

        fn capture_draft(&mut self, _session: &str) -> Option<String> {
            let mut state = self.0.borrow_mut();
            state.record("capture:draft".to_string());
            let count = state.calls.iter().filter(|c| *c == "capture:draft").count();
            if let Some((at, capture)) = &state.draft_capture_at
                && *at == count
            {
                return capture.clone();
            }
            state.capture()
        }

        fn pane_alive(&mut self, _session: &str) -> bool {
            let mut state = self.0.borrow_mut();
            state.record("alive".to_string());
            !state.dead
        }

        fn present(&mut self, _session: &str) -> bool {
            let mut state = self.0.borrow_mut();
            state.record("present".to_string());
            !state.absent
        }

        fn retire(&mut self, _session: &str, reason_code: &str, reason: &str) {
            self.0
                .borrow_mut()
                .record(format!("retire:{reason_code}:{reason}"));
        }
    }

    static DRAFT_ROUTES: std::sync::LazyLock<
        std::sync::Mutex<std::collections::HashMap<String, std::path::PathBuf>>,
    > = std::sync::LazyLock::new(Default::default);

    fn draft_routes()
    -> std::sync::MutexGuard<'static, std::collections::HashMap<String, std::path::PathBuf>> {
        DRAFT_ROUTES.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub(super) fn draft_route(session: &str) -> Option<std::path::PathBuf> {
        draft_routes().get(session).cloned()
    }

    /// A tmux stand-in for one session's draft capture on any thread: `-e` prints the `pane` file
    /// as written, a capture without it drops attributes, and every call is logged.
    #[cfg(unix)]
    pub(crate) struct FakeDraftPane {
        dir: tempfile::TempDir,
        session: String,
    }

    #[cfg(unix)]
    impl FakeDraftPane {
        pub(crate) fn new(session: &str) -> Self {
            use std::os::unix::fs::PermissionsExt;
            let dir = tempfile::tempdir().unwrap();
            let d = dir.path().display();
            let script = format!(
                "#!/bin/sh\necho \"$*\" >> '{d}/log'\ncase \" $* \" in *\" -e \"*) cat '{d}/pane' ;; \
                 *) sed \"s/$(printf '\\033')\\[[0-9;]*m//g\" '{d}/pane' ;; esac\n"
            );
            let program = dir.path().join("tmux");
            std::fs::write(&program, script).unwrap();
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::fs::write(dir.path().join("pane"), "").unwrap();
            draft_routes().insert(session.to_string(), program);
            Self {
                dir,
                session: session.to_string(),
            }
        }

        pub(crate) fn show(&self, pane: &str) {
            std::fs::write(self.dir.path().join("pane"), pane).unwrap();
        }

        /// How many `-e` captures read this pane.
        pub(crate) fn draft_reads(&self) -> usize {
            let log = std::fs::read_to_string(self.dir.path().join("log")).unwrap_or_default();
            log.lines().filter(|line| line.contains(" -e ")).count()
        }
    }

    #[cfg(unix)]
    impl Drop for FakeDraftPane {
        fn drop(&mut self) {
            draft_routes().remove(&self.session);
        }
    }

    /// Routes this thread's legacy input through a spy until dropped.
    pub(crate) struct SpyGuard(pub Rc<RefCell<SpyState>>);

    impl SpyGuard {
        pub(crate) fn install(state: SpyState) -> Self {
            let state = Rc::new(RefCell::new(state));
            let spy: Box<dyn InputTransport> = Box::new(Spy(state.clone()));
            INJECTED.with(|slot| *slot.borrow_mut() = Some(spy));
            Self(state)
        }

        pub(crate) fn calls(&self) -> Vec<String> {
            self.0.borrow().calls.clone()
        }
    }

    impl Drop for SpyGuard {
        fn drop(&mut self) {
            INJECTED.with(|slot| slot.borrow_mut().take());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    use super::spy::{Spy, SpyGuard, SpyState, exit};
    use super::*;
    use crate::services::claude_tui::input::{
        CompactSubmitOutcome, inject_steering_prompt, send_compact_while_busy,
    };
    use crate::services::session_host::{SessionTargetInput, TargetSource, UnknownHost};

    #[test]
    fn guarded_claude_helper_submits_folded_own_draft_and_keeps_late_refusal() {
        let idle = include_str!("../../../tests/fixtures/tui_input/claude-2.1.293-idle.txt");
        let own = "────────────────────\n❯ [Pasted text #8 +2 lines]\n────────────────────\n";
        let frame = "[adk:source:1]\n한글\n[adk:end]";
        for (after, sent) in [(own, true), ("foreign draft", false)] {
            let spy = SpyGuard::install(SpyState {
                captures: [idle, after].map(|s| Some(s.to_string())).into(),
                ..SpyState::default()
            });
            let actions = crate::services::claude_tui::input::plan_prompt_submit(frame).unwrap();
            let run = crate::services::claude_tui::composer_lock::with_composer_mutation_lock(
                "guarded-claude-helper",
                || run_prompt_submission_legacy("guarded-claude-helper", &actions, None),
            );
            assert_eq!(
                spy.calls().iter().filter(|c| *c == "keys:Enter").count(),
                usize::from(sent)
            );
            assert!(
                !spy.calls()
                    .iter()
                    .any(|c| c == "keys:C-u" || c.starts_with("retire:"))
            );
            if sent {
                assert_eq!(run, InputRun::Applied);
            } else {
                assert!(matches!(
                    run,
                    InputRun::Indeterminate {
                        cause: StopCause::Refused(_),
                        ..
                    }
                ));
                assert!(!InputTarget::legacy_tmux("guarded-claude-helper").keys_may_follow(&run));
                let error = run.into_legacy().unwrap_err();
                assert!(error.starts_with("claude tui input held after mutation:"));
                assert!(!crate::services::claude_tui::input::is_prompt_ready_timeout_error(&error));
            }
        }
    }

    #[test]
    fn passive_claude_confirmation_never_retries_or_clears_a_draft() {
        let idle = include_str!("../../../tests/fixtures/tui_input/claude-2.1.293-idle.txt");
        for (capture, confirmed) in [
            (Some("unknown screen"), false),
            (Some("❯ foreign draft"), false),
            (None, false),
            (Some(idle), true),
            (Some("✳ Architecting…"), true),
            (
                Some(
                    "✳ Architecting…\n────────────────────\n❯ [User: A (ID:1)] stranded\n────────────────────\n",
                ),
                false,
            ),
            (
                Some(
                    "Do you want to proceed?\n  1. Yes\n  2. No\nEnter to confirm · esc to cancel\n✳ Architecting…",
                ),
                false,
            ),
            (Some("✳ Architecting…\n❯ \n"), false),
            (
                Some(
                    "✳ Architecting…\n────────────────────\n❯ foreign\n  ❯ \n────────────────────\n",
                ),
                false,
            ),
            (
                Some("✳ Architecting…\n❯ \nunindented continuation\n────────────────────\n"),
                false,
            ),
            (
                Some("✳ Architecting…\n────────────────────\n❯ \n────────────────────\n"),
                true,
            ),
            (
                Some(
                    "❯ [User: A (ID:1)] previous prompt\n✳ Architecting…\n────────────────────\n❯ \n────────────────────\n",
                ),
                true,
            ),
        ] {
            let spy = SpyGuard::install(SpyState {
                captures: [capture.map(str::to_string)].into(),
                ..SpyState::default()
            });
            assert_eq!(
                confirm_prompt_submission_passively("passive-confirmation", None).is_ok(),
                confirmed
            );
            assert!(
                !spy.calls()
                    .iter()
                    .any(|c| c.starts_with("keys:") || c.starts_with("retire:"))
            );
        }
    }

    #[test]
    fn passive_confirmation_checks_cancellation_before_and_after_its_only_capture() {
        for cancel_after_capture in [false, true] {
            let token = Arc::new(CancelToken::new());
            let mut setup = state(&[Some(BUSY)]);
            if cancel_after_capture {
                setup.cancel_on = Some(("capture:draft", 1, token.clone()));
            } else {
                token.cancelled.store(true, Ordering::Relaxed);
            }
            let spy = SpyGuard::install(setup);
            assert_eq!(
                confirm_prompt_submission_passively("passive-cancel", Some(token.as_ref())),
                Err(PROMPT_READY_CANCELLED_ERROR.to_string())
            );
            let calls = spy.calls();
            assert_eq!(
                calls.iter().filter(|c| c.starts_with("capture")).count(),
                usize::from(cancel_after_capture)
            );
            assert!(
                !calls
                    .iter()
                    .any(|c| c.starts_with("keys:") || c.starts_with("retire:"))
            );
            if !cancel_after_capture {
                assert!(calls.is_empty());
            }
        }
    }

    const EMPTY_COMPOSER: &str =
        "Claude Code v2.1.141\n\n────────────────────\n\u{276f} \n────────────────────\nstatus";
    const BUSY: &str = "\u{2733} Architecting\u{2026}";
    const DRAFT: &str = "\u{276f} 남은 초안 한글";

    #[test]
    fn native_clear_indeterminate_never_retries_and_keeps_discord_provenance() {
        let _state = crate::services::tui_prompt_dedupe::TEST_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let session = format!("native-clear-input-{}", uuid::Uuid::new_v4());
        let mut sends = 0;
        let outcome = native_clear_once(
            &session,
            &LegacyTmuxGate,
            tokio::time::Instant::now() + std::time::Duration::from_secs(20),
            |_| Some(EMPTY_COMPOSER.into()),
            |_| None,
            |keys, _| {
                assert_eq!(keys, &["/clear", "Enter"]);
                sends += 1;
                false
            },
        );
        assert_eq!(outcome, NativeClearSubmission::Indeterminate);
        assert_eq!(sends, 1, "Indeterminate must not resend");
        assert_eq!(
            crate::services::tui_prompt_dedupe::observe_prompt_by_tmux(
                "claude", &session, "/clear"
            ),
            crate::services::tui_prompt_dedupe::PromptObservation::SuppressedDiscordDuplicate
        );
    }

    #[test]
    fn native_clear_refuses_drafts_deadlines_and_changed_execution_before_send() {
        let _state = crate::services::tui_prompt_dedupe::TEST_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let session = format!("native-clear-refuse-{}", uuid::Uuid::new_v4());
        for capture in [None, Some(DRAFT), Some(BUSY)] {
            assert_eq!(
                native_clear_once(
                    &session,
                    &LegacyTmuxGate,
                    tokio::time::Instant::now() + std::time::Duration::from_secs(20),
                    |_| capture.map(str::to_owned),
                    |_| None,
                    |_, _| panic!("unready pane sent keys")
                ),
                NativeClearSubmission::NotSent
            );
        }
        assert_eq!(
            native_clear_once(
                &session,
                &LegacyTmuxGate,
                tokio::time::Instant::now(),
                |_| panic!("expired deadline captured pane"),
                |_| None,
                |_, _| panic!("expired deadline sent")
            ),
            NativeClearSubmission::NotSent
        );
        let refused = RefuseAfter(std::cell::Cell::new(0), InputRefusal::IdentityMismatch);
        assert_eq!(
            native_clear_once(
                &session,
                &refused,
                tokio::time::Instant::now() + std::time::Duration::from_secs(20),
                |_| Some(EMPTY_COMPOSER.into()),
                |_| None,
                |_, _| panic!("changed execution sent")
            ),
            NativeClearSubmission::NotSent
        );
        for last in [EMPTY_COMPOSER, DRAFT] {
            let mut captures = [EMPTY_COMPOSER, last].into_iter();
            let result = native_clear_once(
                &session,
                &LegacyTmuxGate,
                tokio::time::Instant::now() + std::time::Duration::from_secs(20),
                |_| captures.next().map(str::to_owned),
                |_| None,
                |_, _| true,
            );
            assert_eq!(
                result,
                if last == EMPTY_COMPOSER {
                    NativeClearSubmission::Confirmed
                } else {
                    NativeClearSubmission::Indeterminate
                }
            );
            crate::services::tui_prompt_dedupe::remove_discord_originated_prompt(
                "claude", &session, "/clear",
            );
        }
    }

    #[test]
    fn native_clear_unsupported_hosts_do_no_tmux_io() {
        for host in [
            known(HostKind::Herdr),
            TargetHost::Unknown(UnknownHost::NoHostEvidence),
        ] {
            let target = ResolvedSessionTarget {
                input: SessionTargetInput::RawName("native-clear-refused".into()),
                session_key: None,
                host,
            };
            assert_eq!(
                super::super::input::submit_native_clear(
                    &InputTarget::from_session_target(&target),
                    &LegacyTmuxGate,
                    tokio::time::Instant::now() + std::time::Duration::from_secs(20)
                ),
                NativeClearSubmission::NotSent
            );
        }
    }

    fn state(captures: &[Option<&str>]) -> SpyState {
        SpyState {
            captures: captures.iter().map(|c| c.map(str::to_string)).collect(),
            ..SpyState::default()
        }
    }

    fn run(spy: &mut Spy, gate: &dyn MutationGate, actions: &[TuiInputAction]) -> InputRun {
        let target = InputTarget::legacy_tmux("p6a1-spy-session");
        run_plan(&target, gate, spy, actions, None)
    }

    fn spy(state: SpyState) -> (Spy, std::rc::Rc<std::cell::RefCell<SpyState>>) {
        let shared = std::rc::Rc::new(std::cell::RefCell::new(state));
        (Spy(shared.clone()), shared)
    }

    struct RefuseAfter(std::cell::Cell<usize>, InputRefusal);

    impl MutationGate for RefuseAfter {
        fn admit(&self, _session: &str) -> Result<(), InputRefusal> {
            let left = self.0.get();
            self.0.set(left.saturating_sub(1));
            if left == 0 { Err(self.1) } else { Ok(()) }
        }
    }

    fn resolved(host: TargetHost) -> InputTarget {
        InputTarget::from_session_target(&ResolvedSessionTarget {
            input: SessionTargetInput::RawName("AgentDesk-claude-p6a1".to_string()),
            session_key: None,
            host,
        })
    }

    fn known(kind: HostKind) -> TargetHost {
        TargetHost::Known {
            kind,
            source: TargetSource::SessionRecord,
            name: "AgentDesk-claude-p6a1".to_string(),
        }
    }

    #[test]
    fn steering_submit_keeps_payload_order_under_both_composer_guards() {
        let name = "p6a1-spy-steering";
        let prompt = format!("첫 줄 한글\n{}", "둘째 줄 긴 붙여넣기 ".repeat(800));
        let own = "────────────────────\n❯ [Pasted text #8 +1 lines]\n────────────────────\n";
        let guard = SpyGuard::install(state(&[
            Some(EMPTY_COMPOSER),
            Some(EMPTY_COMPOSER),
            Some(own),
            Some(BUSY),
        ]));
        assert_eq!(inject_steering_prompt(name, &prompt), Ok(()));
        assert_eq!(
            guard.calls(),
            [
                "capture".to_string(),
                "alive".to_string(),
                "capture:draft".to_string(),
                "alive".to_string(),
                format!("load:{prompt}"),
                "paste:delete=true".to_string(),
                "capture:draft".to_string(),
                "alive".to_string(),
                "keys:Enter".to_string(),
                "capture:draft".to_string(),
                "alive".to_string(),
            ]
        );
        drop(guard);

        let mut failing = state(&[Some(EMPTY_COMPOSER), Some(EMPTY_COMPOSER)]);
        failing.fail_send = Some((1, Ok(exit(1, "no buffer\n"))));
        let guard = SpyGuard::install(failing);
        let error = inject_steering_prompt(name, &prompt).unwrap_err();
        assert!(
            error.starts_with("claude tui input held after mutation:"),
            "{error}"
        );
        assert_eq!(
            guard.calls(),
            [
                "capture".to_string(),
                "alive".to_string(),
                "capture:draft".to_string(),
                "alive".to_string(),
                format!("load:{prompt}"),
                "paste:delete=true".to_string(),
            ]
        );
        drop(guard);

        // Chunking is retained; a busy or unprovable post-payload pane withholds Enter.
        let line = "가".repeat(2000);
        let guard = SpyGuard::install(state(&[
            Some(EMPTY_COMPOSER),
            Some(EMPTY_COMPOSER),
            Some(BUSY),
        ]));
        let error = inject_steering_prompt(name, &line).unwrap_err();
        assert!(error.starts_with("claude tui input held after mutation:"));
        let calls = guard.calls();
        assert_eq!(calls[4], format!("literal:{}", "가".repeat(1800)));
        assert_eq!(calls[5], format!("literal:{}", "가".repeat(200)));
        assert!(!calls.iter().any(|call| call.starts_with("keys:")));
    }

    #[test]
    fn compact_literal_failure_never_sends_enter() {
        let mut failing = state(&[Some(EMPTY_COMPOSER)]);
        failing.fail_send = Some((0, Err("broken pipe".to_string())));
        let guard = SpyGuard::install(failing);
        assert_eq!(
            send_compact_while_busy("p6a1-spy-compact"),
            CompactSubmitOutcome::AmbiguousAfterMutation
        );
        assert_eq!(guard.calls(), ["capture", "alive", "literal:/compact"]);
    }

    #[test]
    fn every_mutation_passes_the_gate_and_a_refusal_stops_before_enter() {
        let plan = [
            TuiInputAction::Literal("a".to_string()),
            TuiInputAction::PasteBuffer("b\nc".to_string()),
            TuiInputAction::Enter,
        ];
        let (mut first, calls) = spy(SpyState::default());
        let refuse = RefuseAfter(0.into(), InputRefusal::IdentityMismatch);
        assert_eq!(
            run(&mut first, &refuse, &plan),
            InputRun::Refused(InputRefusal::IdentityMismatch)
        );
        assert!(
            calls.borrow().calls.is_empty(),
            "nothing may reach the pane"
        );

        // The host is swapped after the literal and the buffer load: no paste, no Enter.
        let (mut swapped, calls) = spy(SpyState::default());
        let refuse = RefuseAfter(2.into(), InputRefusal::IdentityMismatch);
        let stopped = run(&mut swapped, &refuse, &plan);
        assert_eq!(
            stopped,
            InputRun::Indeterminate {
                confirmed: 1,
                cause: StopCause::Refused(InputRefusal::IdentityMismatch),
            }
        );
        assert_eq!(calls.borrow().calls, ["literal:a", "load:b\nc"]);
        let outcome = HostInputOutcome::from_stopped_run(&stopped).unwrap();
        assert!(!outcome.allows_submit() && !outcome.allows_recreate());
    }

    #[test]
    fn a_send_that_may_have_landed_is_indeterminate_and_nothing_follows() {
        let paste = [
            TuiInputAction::PasteBuffer("한글\n줄바꿈".to_string()),
            TuiInputAction::Enter,
        ];
        let literal = [
            TuiInputAction::Literal("x".to_string()),
            TuiInputAction::Enter,
        ];
        let cases: [(
            &[TuiInputAction],
            usize,
            Result<Output, String>,
            usize,
            &str,
        ); 4] = [
            // ACK lost after the paste write.
            (&paste, 1, Err("ack lost".to_string()), 0, "ack lost"),
            (
                &paste,
                0,
                Ok(exit(1, "")),
                0,
                "tmux send paste-buffer failed:",
            ),
            // Enter itself fails after the literal landed.
            (
                &literal,
                1,
                Ok(exit(1, "no pane")),
                1,
                "tmux send enter failed: no pane",
            ),
            // The second backspace batch fails after the first landed.
            (
                &[TuiInputAction::Backspace(40), TuiInputAction::Enter],
                1,
                Ok(exit(1, "gone")),
                1,
                "tmux send backspace failed: gone",
            ),
        ];
        for (plan, at, answer, confirmed, error) in cases {
            let (mut transport, calls) = spy(SpyState {
                fail_send: Some((at, answer)),
                ..SpyState::default()
            });
            let stopped = run(&mut transport, &LegacyTmuxGate, plan);
            let InputRun::Indeterminate { confirmed: got, .. } = &stopped else {
                panic!("{plan:?}: {stopped:?}");
            };
            assert_eq!(*got, confirmed, "{plan:?}");
            assert_eq!(
                calls.borrow().calls.len(),
                at + 1,
                "{plan:?}: nothing after the failure"
            );
            let calls = calls.borrow().calls.clone();
            let enter = calls.iter().position(|c| c == "keys:Enter");
            assert!(enter.is_none_or(|at| at + 1 == calls.len()), "{calls:?}");
            let outcome = HostInputOutcome::from_stopped_run(&stopped).unwrap();
            assert_eq!(outcome, HostInputOutcome::Indeterminate { confirmed });
            assert!(!outcome.allows_submit() && !outcome.allows_recreate());
            assert!(stopped.into_legacy().unwrap_err().starts_with(error));
        }

        // Cancel observed after the first literal keeps the partial count.
        let token = Arc::new(CancelToken::new());
        let (mut transport, calls) = spy(SpyState::default());
        let target = InputTarget::legacy_tmux("p6a1-spy-cancel");
        let plan = [
            TuiInputAction::Literal("a".to_string()),
            TuiInputAction::Literal("b".to_string()),
        ];
        let mut cancelling = CancelAfterFirst(&mut transport, token.clone());
        let stopped = run_plan(
            &target,
            &LegacyTmuxGate,
            &mut cancelling,
            &plan,
            Some(&token),
        );
        assert_eq!(stopped, InputRun::Cancelled { confirmed: 1 });
        assert_eq!(calls.borrow().calls, ["literal:a"]);
        assert_eq!(
            HostInputOutcome::from_stopped_run(&stopped),
            Some(HostInputOutcome::Cancelled)
        );
        assert_eq!(
            stopped.into_legacy(),
            Err(PROMPT_READY_CANCELLED_ERROR.to_string())
        );
    }

    struct CancelAfterFirst<'a>(&'a mut Spy, Arc<CancelToken>);

    impl InputTransport for CancelAfterFirst<'_> {
        fn send_literal(&mut self, session: &str, text: &str) -> Result<Output, String> {
            self.1.cancelled.store(true, Ordering::Relaxed);
            self.0.send_literal(session, text)
        }
        fn load_buffer(&mut self, b: &str, t: &str) -> Result<Output, String> {
            self.0.load_buffer(b, t)
        }
        fn paste_buffer(&mut self, s: &str, b: &str, d: bool) -> Result<Output, String> {
            self.0.paste_buffer(s, b, d)
        }
        fn send_keys(&mut self, s: &str, k: &[HostKey]) -> Result<Output, String> {
            self.0.send_keys(s, k)
        }
        fn capture(&mut self, s: &str, b: i32) -> Option<String> {
            self.0.capture(s, b)
        }
        fn pane_alive(&mut self, s: &str) -> bool {
            self.0.pane_alive(s)
        }
        fn present(&mut self, s: &str) -> bool {
            self.0.present(s)
        }
        fn retire(&mut self, s: &str, c: &str, r: &str) {
            self.0.retire(s, c, r)
        }
    }

    #[test]
    fn non_tmux_targets_send_nothing_and_are_never_recreated() {
        let conflict = TargetHost::Conflict {
            first: (HostKind::Herdr, TargetSource::SessionRecord),
            second: (HostKind::Tmux, TargetSource::InflightLocator),
        };
        let refused = [
            (
                known(HostKind::Herdr),
                InputRefusal::Unsupported(HostKind::Herdr),
            ),
            (
                known(HostKind::Process),
                InputRefusal::Unsupported(HostKind::Process),
            ),
            (
                TargetHost::Unknown(UnknownHost::NoHostEvidence),
                InputRefusal::Unknown,
            ),
            (conflict, InputRefusal::Conflict),
        ];
        let dead = HostCapture::Complete(EMPTY_COMPOSER.to_string());
        for (host, refusal) in refused {
            let target = resolved(host);
            assert_eq!(target, InputTarget::Refused(refusal));
            let (mut transport, calls) = spy(SpyState::default());
            let plan = [TuiInputAction::CtrlU, TuiInputAction::Enter];
            let run = run_plan(&target, &LegacyTmuxGate, &mut transport, &plan, None);
            assert_eq!(run, InputRun::Refused(refusal));
            assert!(calls.borrow().calls.is_empty(), "{refusal:?}: no key");
            let outcome = classify(&target, SessionLiveness::Missing, &dead);
            assert_eq!(outcome, HostInputOutcome::Refused(refusal));
            assert!(!outcome.allows_recreate(), "{refusal:?}");
        }

        let tmux = resolved(known(HostKind::Tmux));
        assert_eq!(tmux, InputTarget::Tmux("AgentDesk-claude-p6a1".to_string()));
        let outcome = classify(&tmux, SessionLiveness::Missing, &dead);
        assert_eq!(outcome, HostInputOutcome::LegacyRecreateEligible);
        assert!(outcome.allows_recreate() && !outcome.allows_submit());
    }

    #[test]
    fn blind_truncated_or_failed_reads_are_never_ready() {
        let tmux = InputTarget::legacy_tmux("p6a1-classify");
        let ready = HostCapture::Complete(EMPTY_COMPOSER.to_string());
        for liveness in [SessionLiveness::ProbeFailed, SessionLiveness::Unknown] {
            let outcome = classify(&tmux, liveness, &ready);
            assert_eq!(outcome, HostInputOutcome::Refused(InputRefusal::Unknown));
            assert!(!outcome.allows_submit() && !outcome.allows_recreate());
        }
        for capture in [HostCapture::Truncated, HostCapture::Unavailable] {
            let outcome = classify(&tmux, SessionLiveness::Alive, &capture);
            assert_eq!(outcome, HostInputOutcome::UnknownTranscript, "{capture:?}");
        }
        let alive = |pane: &str| {
            classify(
                &tmux,
                SessionLiveness::Alive,
                &HostCapture::Complete(pane.to_string()),
            )
        };
        assert_eq!(alive(EMPTY_COMPOSER), HostInputOutcome::ReadySameExecution);
        assert_eq!(alive(DRAFT), HostInputOutcome::PersistentDraft);
        assert_eq!(alive(BUSY), HostInputOutcome::Busy);

        let cleared = HostInputOutcome::after_clear(&InputRun::Applied, alive(EMPTY_COMPOSER));
        assert_eq!(cleared, HostInputOutcome::Cleared);
        let kept = HostInputOutcome::after_clear(&InputRun::Applied, alive(DRAFT));
        assert_eq!(kept, HostInputOutcome::PersistentDraft);
        let partial = InputRun::Indeterminate {
            confirmed: 1,
            cause: StopCause::Send("ack lost".to_string()),
        };
        assert_eq!(
            HostInputOutcome::after_clear(&partial, alive(EMPTY_COMPOSER)),
            HostInputOutcome::Indeterminate { confirmed: 1 }
        );
    }

    #[test]
    fn a_mounted_dialog_is_never_ready_even_after_clear() {
        let tmux = InputTarget::legacy_tmux("p6a2-modal");
        for pane in [
            "Ready for input (type message + Enter)\nAllow / Deny\nEnter to confirm",
            "Claude Code v2.1.141\n\n\u{276f} \nAllow / Deny\nEnter to confirm",
            "Do you want to make this edit?\n\u{276f} 1. Yes\n  2. No\nEnter to confirm \u{b7} Esc to cancel",
        ] {
            let capture = HostCapture::Complete(pane.to_string());
            let outcome = classify(&tmux, SessionLiveness::Alive, &capture);
            assert!(!outcome.allows_submit(), "{pane:?}: {outcome:?}");
            let cleared = HostInputOutcome::after_clear(&InputRun::Applied, outcome);
            assert!(!cleared.allows_submit(), "{pane:?}: {cleared:?}");
        }
    }

    #[test]
    fn draft_cleanup_follows_a_stop_only_on_a_confirmed_tmux_session() {
        let plan = [
            TuiInputAction::Literal("a".to_string()),
            TuiInputAction::PasteBuffer("b\nc".to_string()),
            TuiInputAction::Enter,
        ];
        let tmux = InputTarget::legacy_tmux("p6a2-cleanup");
        let (mut failing, _) = spy(SpyState {
            fail_send: Some((2, Ok(exit(1, "no buffer")))),
            ..SpyState::default()
        });
        let send_failure = run_plan(&tmux, &LegacyTmuxGate, &mut failing, &plan, None);
        assert!(matches!(
            send_failure,
            InputRun::Indeterminate {
                cause: StopCause::Send(_),
                ..
            }
        ));
        assert!(tmux.keys_may_follow(&send_failure));
        assert!(resolved(known(HostKind::Tmux)).keys_may_follow(&send_failure));

        // A gate refusal means the host changed: no cleanup key follows.
        for admitted in [0, 2] {
            let (mut swapped, _) = spy(SpyState::default());
            let gate = RefuseAfter(admitted.into(), InputRefusal::IdentityMismatch);
            let stopped = run_plan(&tmux, &gate, &mut swapped, &plan, None);
            assert!(!tmux.keys_may_follow(&stopped), "{stopped:?}");
        }
        for target in [
            resolved(known(HostKind::Herdr)),
            resolved(TargetHost::Unknown(UnknownHost::NoHostEvidence)),
        ] {
            for run in [send_failure.clone(), InputRun::Cancelled { confirmed: 1 }] {
                assert!(!target.keys_may_follow(&run), "{target:?} {run:?}");
            }
        }
    }
}
