//! Host executor for Codex TUI input: pane reads, key plans and the legacy kill
//! barrier run through one transport, and only a confirmed tmux session takes them.

use std::process::Output;
use std::time::Duration;

use super::input::{PROMPT_READY_CANCELLED_ERROR, TuiInputAction};
pub(crate) use crate::services::claude_tui::host_input::InputRefusal;
use crate::services::claude_tui::host_input::{HerdrInput, InputTransport};
pub(crate) use crate::services::claude_tui::host_input::{
    InputRun, InputTarget, LegacyTmuxGate, MutationGate, StopCause,
};
use crate::services::platform::tmux;
use crate::services::process::ProcessIdentity;
use crate::services::provider::{CancelToken, cancel_requested};
#[cfg(unix)]
use crate::services::session_host::HerdrTarget;
use crate::services::session_host::{HostKey, TmuxHost};
use crate::services::tui_input::submission_tmux::SubmissionTmux;

const PROMPT_INPUT_BEFORE_ENTER_SETTLE: Duration = Duration::from_millis(200);

/// Pane writes a Codex key plan makes.
pub(crate) trait CodexWrites {
    fn send_literal(&mut self, session: &str, text: &str) -> Result<Output, String>;
    fn load_buffer(&mut self, buffer: &str, text: &str) -> Result<Output, String>;
    fn paste_buffer(&mut self, session: &str, buffer: &str, delete: bool)
    -> Result<Output, String>;
    fn send_keys(&mut self, session: &str, keys: &[HostKey]) -> Result<Output, String>;
}

/// Codex pane reads and the legacy kill barrier. Codex keeps its own transport
/// because the Claude one also retires sessions under the Claude termination owner.
pub(crate) trait CodexTransport: CodexWrites {
    fn pane_size(&mut self, _session: &str) -> Option<(usize, usize)> {
        None
    }

    fn capture_ansi(&mut self, session: &str, scroll_back: i32) -> Option<String>;
    fn capture_bounded(
        &mut self,
        session: &str,
        scroll_back: i32,
        timeout: Duration,
    ) -> Option<String>;
    fn pane_alive(&mut self, session: &str) -> bool;
    fn pane_pid(&mut self, session: &str) -> Option<u32>;
    fn kill_tree(&mut self, pid: u32, identity: ProcessIdentity) -> bool;
    fn kill_session(&mut self, session: &str, reason: &str) -> bool;
    #[cfg(unix)]
    fn pane_stopped(&mut self, session: &str, pid: u32, identity: ProcessIdentity) -> bool;
}

struct TmuxCodexInput;

/// Herdr writes as the Claude executor makes them: a buffer stays in memory until its paste.
impl CodexWrites for HerdrInput<'_> {
    fn send_literal(&mut self, session: &str, text: &str) -> Result<Output, String> {
        InputTransport::send_literal(self, session, text)
    }

    fn load_buffer(&mut self, buffer: &str, text: &str) -> Result<Output, String> {
        InputTransport::load_buffer(self, buffer, text)
    }

    fn paste_buffer(&mut self, s: &str, buffer: &str, delete: bool) -> Result<Output, String> {
        InputTransport::paste_buffer(self, s, buffer, delete)
    }

    fn send_keys(&mut self, session: &str, keys: &[HostKey]) -> Result<Output, String> {
        InputTransport::send_keys(self, session, keys)
    }
}

impl CodexWrites for TmuxCodexInput {
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
}

impl CodexWrites for SubmissionTmux {
    fn send_literal(&mut self, session: &str, text: &str) -> Result<Output, String> {
        InputTransport::send_literal(self, session, text)
    }

    fn load_buffer(&mut self, buffer: &str, text: &str) -> Result<Output, String> {
        InputTransport::load_buffer(self, buffer, text)
    }

    fn paste_buffer(
        &mut self,
        session: &str,
        buffer: &str,
        delete: bool,
    ) -> Result<Output, String> {
        InputTransport::paste_buffer(self, session, buffer, delete)
    }

    fn send_keys(&mut self, session: &str, keys: &[HostKey]) -> Result<Output, String> {
        InputTransport::send_keys(self, session, keys)
    }
}

impl CodexTransport for SubmissionTmux {
    fn pane_size(&mut self, session: &str) -> Option<(usize, usize)> {
        SubmissionTmux::pane_size(self, session)
    }

    fn capture_ansi(&mut self, session: &str, scroll_back: i32) -> Option<String> {
        InputTransport::capture(self, session, scroll_back)
    }

    fn capture_bounded(
        &mut self,
        session: &str,
        scroll_back: i32,
        _timeout: Duration,
    ) -> Option<String> {
        InputTransport::capture(self, session, scroll_back)
    }

    fn pane_alive(&mut self, session: &str) -> bool {
        InputTransport::pane_alive(self, session)
    }

    fn pane_pid(&mut self, _session: &str) -> Option<u32> {
        None
    }

    fn kill_tree(&mut self, _pid: u32, _identity: ProcessIdentity) -> bool {
        false
    }

    fn kill_session(&mut self, _session: &str, _reason: &str) -> bool {
        false
    }

    #[cfg(unix)]
    fn pane_stopped(&mut self, _session: &str, _pid: u32, _identity: ProcessIdentity) -> bool {
        false
    }
}

impl CodexTransport for TmuxCodexInput {
    fn capture_ansi(&mut self, session: &str, scroll_back: i32) -> Option<String> {
        tmux::capture_pane_with_escapes(session, scroll_back)
    }

    fn capture_bounded(
        &mut self,
        session: &str,
        scroll_back: i32,
        timeout: Duration,
    ) -> Option<String> {
        tmux::capture_pane_timeout(session, scroll_back, timeout)
    }

    fn pane_alive(&mut self, session: &str) -> bool {
        crate::services::tmux_diagnostics::tmux_session_has_live_pane(session)
    }

    fn pane_pid(&mut self, session: &str) -> Option<u32> {
        tmux::pane_pid(session)
    }

    fn kill_tree(&mut self, pid: u32, identity: ProcessIdentity) -> bool {
        crate::services::process::kill_pid_tree_if_identity_matches(pid, identity)
    }

    fn kill_session(&mut self, session: &str, reason: &str) -> bool {
        tmux::kill_session(session, reason)
    }

    #[cfg(unix)]
    fn pane_stopped(&mut self, session: &str, pid: u32, identity: ProcessIdentity) -> bool {
        use crate::services::process::{ProcessGroupProbe, ProcessIdentityProbe};
        let pane_stopped = matches!(
            crate::services::tmux_diagnostics::tmux_session_pane_liveness(session),
            tmux::PaneLiveness::DeadOrAbsent
        );
        let process_stopped = matches!(identity.probe(pid), ProcessIdentityProbe::GoneOrReused);
        let process_group_stopped = matches!(
            crate::services::process::process_group_probe(pid),
            ProcessGroupProbe::Gone
        );
        pane_stopped && process_stopped && process_group_stopped
    }
}

#[cfg(test)]
type Injected = (Box<dyn CodexTransport>, Box<dyn MutationGate>);

#[cfg(test)]
thread_local! {
    static INJECTED: std::cell::RefCell<Option<Injected>> = const { std::cell::RefCell::new(None) };
}

fn with_legacy<R>(operation: impl FnOnce(&mut dyn CodexTransport, &dyn MutationGate) -> R) -> R {
    with_legacy_using(&mut TmuxCodexInput, operation)
}

fn with_legacy_using<R>(
    default_transport: &mut dyn CodexTransport,
    operation: impl FnOnce(&mut dyn CodexTransport, &dyn MutationGate) -> R,
) -> R {
    #[cfg(test)]
    if let Some((mut transport, gate)) = INJECTED.with(|slot| slot.borrow_mut().take()) {
        let result = operation(transport.as_mut(), gate.as_ref());
        INJECTED.with(|slot| *slot.borrow_mut() = Some((transport, gate)));
        return result;
    }
    operation(default_transport, &LegacyTmuxGate)
}

/// A plan's stop point plus what the legacy submit reads from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlanRun {
    pub(crate) run: InputRun,
    pub(crate) composer_mutated: bool,
    pub(crate) enter_attempted: bool,
}

/// Runs `actions` on `target`, admitting every pane mutation through `gate`.
/// The first failure or refusal stops the plan; nothing after it is sent.
pub(crate) fn run_plan(
    target: &InputTarget,
    gate: &dyn MutationGate,
    transport: &mut dyn CodexWrites,
    actions: &[TuiInputAction],
    cancel_token: Option<&CancelToken>,
) -> PlanRun {
    let session = match target.tmux_session() {
        Ok(session) => session,
        Err(refusal) => {
            return PlanRun {
                run: InputRun::Refused(refusal),
                composer_mutated: false,
                enter_attempted: false,
            };
        }
    };
    run_on(session, gate, transport, actions, cancel_token)
}

/// A plan on a Herdr pane: its own gate admits each write, and only a confirmed write succeeds.
#[cfg(unix)]
pub(crate) fn run_herdr(
    target: &HerdrTarget,
    actions: &[TuiInputAction],
    cancel_token: Option<&CancelToken>,
) -> PlanRun {
    let mut input = HerdrInput::new(target);
    run_on(target.pane_id(), target, &mut input, actions, cancel_token)
}

fn run_on(
    session: &str,
    gate: &dyn MutationGate,
    transport: &mut dyn CodexWrites,
    actions: &[TuiInputAction],
    cancel_token: Option<&CancelToken>,
) -> PlanRun {
    let mut plan = Plan {
        session,
        gate,
        transport,
        cancel_token,
        confirmed: 0,
        composer_mutated: false,
        enter_attempted: false,
    };
    let run = match actions.iter().try_for_each(|action| plan.step(action)) {
        Ok(()) => InputRun::Applied,
        Err(stopped) => stopped,
    };
    PlanRun {
        run,
        composer_mutated: plan.composer_mutated,
        enter_attempted: plan.enter_attempted,
    }
}

struct Plan<'a> {
    session: &'a str,
    gate: &'a dyn MutationGate,
    transport: &'a mut dyn CodexWrites,
    cancel_token: Option<&'a CancelToken>,
    confirmed: usize,
    composer_mutated: bool,
    enter_attempted: bool,
}

impl Plan<'_> {
    fn step(&mut self, action: &TuiInputAction) -> Result<(), InputRun> {
        self.check_cancel()?;
        let session = self.session;
        match action {
            TuiInputAction::Literal(text) => self.mutate(action, |t| t.send_literal(session, text)),
            TuiInputAction::PasteBuffer(text) => {
                let buffer = format!("agentdesk-codex-tui-input-{}", uuid::Uuid::new_v4());
                self.admit()?;
                self.send(action, |t| t.load_buffer(&buffer, text))?;
                self.check_cancel()?;
                self.mutate(action, |t| t.paste_buffer(session, &buffer, true))
            }
            TuiInputAction::Enter => {
                // Let the composer apply the last write before Enter, then re-check
                // cancellation so /stop inside the settle never submits.
                std::thread::sleep(PROMPT_INPUT_BEFORE_ENTER_SETTLE);
                self.check_cancel()?;
                self.mutate(action, |t| t.send_keys(session, &[HostKey::Enter]))
            }
            TuiInputAction::Escape => {
                self.mutate(action, |t| t.send_keys(session, &[HostKey::Escape]))
            }
        }
    }

    fn check_cancel(&self) -> Result<(), InputRun> {
        if cancel_requested(self.cancel_token) {
            return Err(InputRun::Cancelled {
                confirmed: self.confirmed,
            });
        }
        Ok(())
    }

    fn admit(&self) -> Result<(), InputRun> {
        self.gate
            .admit(self.session)
            .map_err(|refusal| match self.confirmed {
                0 => InputRun::Refused(refusal),
                confirmed => InputRun::Indeterminate {
                    confirmed,
                    cause: StopCause::Refused(refusal),
                },
            })
    }

    fn send(
        &mut self,
        action: &TuiInputAction,
        send: impl FnOnce(&mut dyn CodexWrites) -> Result<Output, String>,
    ) -> Result<(), InputRun> {
        send(&mut *self.transport)
            .and_then(|output| ensure_tmux_success(output, action))
            .map_err(|error| InputRun::Indeterminate {
                confirmed: self.confirmed,
                cause: StopCause::Send(error),
            })
    }

    fn mutate(
        &mut self,
        action: &TuiInputAction,
        send: impl FnOnce(&mut dyn CodexWrites) -> Result<Output, String>,
    ) -> Result<(), InputRun> {
        self.admit()?;
        self.enter_attempted |= matches!(action, TuiInputAction::Enter);
        self.send(action, send)?;
        self.composer_mutated |= !matches!(action, TuiInputAction::Enter | TuiInputAction::Escape);
        self.confirmed += 1;
        Ok(())
    }
}

fn ensure_tmux_success(output: Output, action: &TuiInputAction) -> Result<(), String> {
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let action_name = match action {
        TuiInputAction::Literal(_) => "literal",
        TuiInputAction::PasteBuffer(_) => "paste-buffer",
        TuiInputAction::Enter => "enter",
        TuiInputAction::Escape => "escape",
    };
    if stderr.is_empty() {
        Err(format!("tmux send {action_name} failed: {}", output.status))
    } else {
        Err(format!("tmux send {action_name} failed: {stderr}"))
    }
}

/// True when the gate, not a send, stopped the run; such a stop is never retried.
pub(crate) fn refused_by_gate(run: &InputRun) -> bool {
    matches!(
        run,
        InputRun::Refused(_)
            | InputRun::Indeterminate {
                cause: StopCause::Refused(_),
                ..
            }
    )
}

pub(crate) fn refusal_error(run: &InputRun) -> String {
    match run {
        InputRun::Indeterminate { .. } => format!("codex tui input held after mutation: {run:?}"),
        _ => format!("codex tui input refused before mutation: {run:?}"),
    }
}

/// The `Result` legacy tmux callers read, with their original error text.
pub(crate) fn legacy_result(run: InputRun) -> Result<(), String> {
    match run {
        InputRun::Applied => Ok(()),
        InputRun::Cancelled { .. } => Err(PROMPT_READY_CANCELLED_ERROR.to_string()),
        InputRun::Indeterminate {
            cause: StopCause::Send(error),
            ..
        } => Err(error),
        stopped => Err(refusal_error(&stopped)),
    }
}

struct SubmissionWrites<'a> {
    transport: &'a mut dyn CodexTransport,
    attempted: &'a std::cell::Cell<bool>,
}

impl CodexWrites for SubmissionWrites<'_> {
    fn send_literal(&mut self, session: &str, text: &str) -> Result<Output, String> {
        self.attempted.set(true);
        self.transport.send_literal(session, text)
    }
    fn load_buffer(&mut self, buffer: &str, text: &str) -> Result<Output, String> {
        self.transport.load_buffer(buffer, text)
    }
    fn paste_buffer(
        &mut self,
        session: &str,
        buffer: &str,
        delete: bool,
    ) -> Result<Output, String> {
        self.attempted.set(true);
        self.transport.paste_buffer(session, buffer, delete)
    }
    fn send_keys(&mut self, session: &str, keys: &[HostKey]) -> Result<Output, String> {
        self.transport.send_keys(session, keys)
    }
}

/// Submit one guarded prompt; the caller holds the composer lock across these steps.
pub(crate) fn run_prompt_submission_legacy(
    session_name: &str,
    actions: &[TuiInputAction],
    before: Option<&str>,
    cancel_token: Option<&CancelToken>,
) -> PlanRun {
    use std::cell::{Cell, RefCell};

    use crate::services::tui_input::submission::{
        Refusal, Submission, run_prompt_submission_using,
    };
    use crate::services::tui_o::shadow::ShadowProvider;

    let invalid = || PlanRun {
        run: InputRun::Refused(InputRefusal::Composer(Refusal::InvalidPrompt)),
        composer_mutated: false,
        enter_attempted: false,
    };
    let Some((TuiInputAction::Enter, payload)) = actions.split_last() else {
        return invalid();
    };
    let mut frame = String::new();
    for action in payload {
        match action {
            TuiInputAction::Literal(text) | TuiInputAction::PasteBuffer(text) => {
                frame.push_str(text);
            }
            _ => return invalid(),
        }
    }
    let target = InputTarget::legacy_tmux(session_name);
    with_legacy_using(&mut SubmissionTmux, |transport, gate| {
        let transport = RefCell::new(transport);
        let payload_run = RefCell::new(PlanRun {
            run: InputRun::Applied,
            composer_mutated: false,
            enter_attempted: false,
        });
        let stopped = RefCell::new(None);
        let composer_attempted = Cell::new(false);
        let enter_attempted = Cell::new(false);
        let size = Cell::new(None);
        let run = run_prompt_submission_using(
            Submission {
                provider: ShadowProvider::Codex,
                frame: &frame,
                before,
                mutations: payload.len(),
            },
            cancel_token,
            || {
                let mut transport = transport.borrow_mut();
                let current_size = transport.pane_size(session_name);
                size.set(current_size);
                if cancel_requested(cancel_token) {
                    return InputRun::Cancelled { confirmed: 0 };
                }
                let uses_paste = payload
                    .iter()
                    .any(|action| matches!(action, TuiInputAction::PasteBuffer(_)));
                if !super::input::prompt_submission_fits_pane(&frame, uses_paste, current_size) {
                    return InputRun::Refused(InputRefusal::Composer(Refusal::UnpredictableRender));
                }
                let mut writes = SubmissionWrites {
                    transport: &mut **transport,
                    attempted: &composer_attempted,
                };
                let mut result = run_plan(&target, gate, &mut writes, payload, cancel_token);
                // An unacknowledged pane write can have landed; only load-only proves no effect.
                result.composer_mutated |= composer_attempted.get();
                let run = result.run.clone();
                *payload_run.borrow_mut() = result;
                run
            },
            || {
                // Settle before the fresh capture, so no delay separates its guard from Enter.
                std::thread::sleep(PROMPT_INPUT_BEFORE_ENTER_SETTLE);
                if cancel_requested(cancel_token) {
                    *stopped.borrow_mut() = Some(InputRun::Cancelled {
                        confirmed: payload.len(),
                    });
                    return None;
                }
                if let Err(refusal) = gate.admit(session_name) {
                    *stopped.borrow_mut() = Some(if cancel_requested(cancel_token) {
                        InputRun::Cancelled {
                            confirmed: payload.len(),
                        }
                    } else {
                        InputRun::Indeterminate {
                            confirmed: payload.len(),
                            cause: StopCause::Refused(refusal),
                        }
                    });
                    return None;
                }
                let mut transport = transport.borrow_mut();
                if transport.pane_size(session_name) != size.get()
                    || !transport.pane_alive(session_name)
                {
                    return None;
                }
                transport.capture_ansi(session_name, -80)
            },
            || {
                let mut transport = transport.borrow_mut();
                let mut plan = Plan {
                    session: session_name,
                    gate,
                    transport: &mut **transport,
                    cancel_token,
                    confirmed: 0,
                    composer_mutated: false,
                    enter_attempted: false,
                };
                let run = match plan.check_cancel().and_then(|()| {
                    plan.mutate(&TuiInputAction::Enter, |transport| {
                        transport.send_keys(session_name, &[HostKey::Enter])
                    })
                }) {
                    Ok(()) => InputRun::Applied,
                    Err(run) => run,
                };
                enter_attempted.set(plan.enter_attempted);
                run
            },
        );
        PlanRun {
            run: stopped.into_inner().unwrap_or(run),
            composer_mutated: payload_run.into_inner().composer_mutated,
            enter_attempted: enter_attempted.get(),
        }
    })
}

/// One key write on the legacy tmux session, admitted by the gate first.
pub(crate) fn legacy_keys(session_name: &str, keys: &[HostKey]) -> Result<Output, String> {
    with_legacy(|transport, gate| {
        gate.admit(session_name)
            .map_err(|refusal| refusal_error(&InputRun::Refused(refusal)))?;
        transport.send_keys(session_name, keys)
    })
}

/// ANSI capture, then pane liveness: the order the readiness snapshot reads them.
pub(crate) fn observe_legacy(session_name: &str, scroll_back: i32) -> (Option<String>, bool) {
    with_legacy(|transport, _| {
        let capture = transport.capture_ansi(session_name, scroll_back);
        (capture, transport.pane_alive(session_name))
    })
}

pub(crate) fn observe_prompt_submission(
    session_name: &str,
    scroll_back: i32,
) -> (Option<String>, bool) {
    with_legacy_using(&mut SubmissionTmux, |transport, _| {
        let capture = transport.capture_ansi(session_name, scroll_back);
        (capture, transport.pane_alive(session_name))
    })
}

pub(crate) fn capture_bounded(
    session_name: &str,
    scroll_back: i32,
    timeout: Duration,
) -> Option<String> {
    with_legacy(|transport, _| transport.capture_bounded(session_name, scroll_back, timeout))
}

pub(crate) fn legacy_pane_alive(session_name: &str) -> bool {
    with_legacy(|transport, _| transport.pane_alive(session_name))
}

/// Kill barrier for a stranded draft: pin the pane PID, kill its tree and the
/// session, then wait for pane, process and group death. Only tmux is killed.
#[cfg(unix)]
pub(crate) fn kill_legacy_pane(target: &InputTarget, reason_text: &str) -> Result<(), String> {
    let InputTarget::Tmux(session) = target else {
        return Err(format!(
            "Codex TUI warm follow-up refused to kill a non-tmux target: {target:?}"
        ));
    };
    with_legacy(|transport, _| kill_and_confirm_stopped(transport, session, reason_text))
}

#[cfg(unix)]
fn kill_and_confirm_stopped(
    transport: &mut dyn CodexTransport,
    tmux_session_name: &str,
    reason_text: &str,
) -> Result<(), String> {
    let pane_pid = transport.pane_pid(tmux_session_name).ok_or_else(|| {
        "Codex TUI warm follow-up could not pin the pane PID before kill".to_string()
    })?;
    let pane_identity = ProcessIdentity::capture(pane_pid);
    let process_tree_kill_started = transport.kill_tree(pane_pid, pane_identity);
    let kill_succeeded = transport.kill_session(tmux_session_name, reason_text);
    if !process_tree_kill_started || !kill_succeeded {
        tracing::warn!(
            tmux_session_name,
            pane_pid,
            process_tree_kill_started,
            kill_succeeded,
            "Codex TUI warm follow-up kill command was incomplete; requiring independent pane, PID, and process-group death proof"
        );
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        if transport.pane_stopped(tmux_session_name, pane_pid, pane_identity) {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Err("Codex TUI warm follow-up could not prove pane, process, and process-group termination after fallback kill barrier".to_string())
}

/// The write-only interface the plan tests drive the executor through.
#[cfg(test)]
pub(super) trait TuiActionExecutor {
    fn send_literal(&mut self, session_name: &str, text: &str) -> Result<Output, String>;
    fn load_buffer(&mut self, buffer_name: &str, text: &str) -> Result<Output, String>;
    fn paste_buffer(
        &mut self,
        session_name: &str,
        buffer_name: &str,
        delete: bool,
    ) -> Result<Output, String>;
    fn send_keys(&mut self, session_name: &str, keys: &[&str]) -> Result<Output, String>;
}

#[cfg(test)]
pub(super) fn run_actions_with_executor(
    session_name: &str,
    actions: &[TuiInputAction],
    cancel_token: Option<&CancelToken>,
    executor: &mut impl TuiActionExecutor,
) -> Result<(), String> {
    let target = InputTarget::legacy_tmux(session_name);
    let mut writes = ExecutorWrites(executor);
    legacy_result(run_plan(&target, &LegacyTmuxGate, &mut writes, actions, cancel_token).run)
}

#[cfg(test)]
struct ExecutorWrites<'a, E>(&'a mut E);

#[cfg(test)]
impl<E: TuiActionExecutor> CodexWrites for ExecutorWrites<'_, E> {
    fn send_literal(&mut self, session: &str, text: &str) -> Result<Output, String> {
        self.0.send_literal(session, text)
    }

    fn load_buffer(&mut self, buffer: &str, text: &str) -> Result<Output, String> {
        self.0.load_buffer(buffer, text)
    }

    fn paste_buffer(
        &mut self,
        session: &str,
        buffer: &str,
        delete: bool,
    ) -> Result<Output, String> {
        self.0.paste_buffer(session, buffer, delete)
    }

    fn send_keys(&mut self, session: &str, keys: &[HostKey]) -> Result<Output, String> {
        let names: Vec<&str> = keys
            .iter()
            .map(|key| crate::services::session_host::tmux_key_name(*key))
            .collect();
        self.0.send_keys(session, &names)
    }
}

/// Recording transport and gate the Codex TUI tests install in place of tmux.
#[cfg(test)]
pub(super) mod spy {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    use super::*;
    use crate::services::session_host::{
        HostKind, ResolvedSessionTarget, SessionTargetInput, TargetHost, TargetSource, UnknownHost,
        tmux_key_name,
    };

    pub(crate) struct SpyState {
        pub calls: Vec<String>,
        /// Answer for the n-th send (load, literal, paste or keys), counted from 0.
        pub fail_send: Option<(usize, Result<Output, String>)>,
        /// Token flipped right after the n-th send.
        pub cancel_after_send: Option<(usize, Arc<CancelToken>)>,
        /// Gate admissions granted before the gate refuses.
        pub refuse_after: Option<(usize, InputRefusal)>,
        pub captures: VecDeque<Option<String>>,
        pub buffers: Vec<String>,
        pub dead: bool,
        pub dead_after_send: Option<usize>,
        pub pane_size: Option<(usize, usize)>,
        /// Geometry read once a payload send happened, as when the pane resizes mid-submit.
        pub pane_size_after_send: Option<Option<(usize, usize)>>,
        pub cancel_after_size: Option<Arc<CancelToken>>,
        pub pane_pid: Option<u32>,
        pub sends: usize,
        pub assert_composer_lock: Option<String>,
    }

    impl Default for SpyState {
        fn default() -> Self {
            Self {
                calls: Vec::new(),
                fail_send: None,
                cancel_after_send: None,
                refuse_after: None,
                captures: VecDeque::new(),
                buffers: Vec::new(),
                dead: false,
                dead_after_send: None,
                pane_size: Some((80, 24)),
                pane_size_after_send: None,
                cancel_after_size: None,
                pane_pid: None,
                sends: 0,
                assert_composer_lock: None,
            }
        }
    }

    pub(crate) struct Spy(pub Rc<RefCell<SpyState>>);

    impl Spy {
        fn assert_lock(&self) {
            if let Some(session) = self.0.borrow().assert_composer_lock.as_deref() {
                std::thread::scope(|scope| {
                    assert!(
                        scope
                            .spawn(|| {
                                crate::services::codex_tui::input::try_with_composer_mutation_lock(
                                    session,
                                    || (),
                                )
                            })
                            .join()
                            .unwrap()
                            .is_none(),
                        "competing writer entered during submission"
                    );
                });
            }
        }

        fn send(&mut self, call: String) -> Result<Output, String> {
            self.assert_lock();
            let mut state = self.0.borrow_mut();
            state.calls.push(call);
            let index = state.sends;
            state.sends += 1;
            if let Some((at, token)) = &state.cancel_after_send
                && *at == index
            {
                token.cancelled.store(true, Ordering::Relaxed);
            }
            match state.fail_send.take() {
                Some((at, answer)) if at == index => answer,
                other => {
                    state.fail_send = other;
                    Ok(exit(0, ""))
                }
            }
        }

        fn read(&mut self, call: &str) -> Option<String> {
            self.assert_lock();
            let mut state = self.0.borrow_mut();
            state.calls.push(call.to_string());
            state.captures.pop_front().flatten()
        }

        fn record(&mut self, call: &str) -> std::cell::RefMut<'_, SpyState> {
            self.assert_lock();
            let mut state = self.0.borrow_mut();
            state.calls.push(call.to_string());
            state
        }
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

    impl CodexWrites for Spy {
        fn send_literal(&mut self, _session: &str, text: &str) -> Result<Output, String> {
            self.send(format!("literal:{text}"))
        }

        fn load_buffer(&mut self, buffer: &str, text: &str) -> Result<Output, String> {
            self.0.borrow_mut().buffers.push(buffer.to_string());
            self.send(format!("load:{text}"))
        }

        fn paste_buffer(&mut self, _s: &str, buffer: &str, delete: bool) -> Result<Output, String> {
            self.0.borrow_mut().buffers.push(buffer.to_string());
            self.send(format!("paste:delete={delete}"))
        }

        fn send_keys(&mut self, _session: &str, keys: &[HostKey]) -> Result<Output, String> {
            let names: Vec<&str> = keys.iter().map(|key| tmux_key_name(*key)).collect();
            self.send(format!("keys:{}", names.join("+")))
        }
    }

    impl CodexTransport for Spy {
        fn pane_size(&mut self, _session: &str) -> Option<(usize, usize)> {
            let state = self.record("pane_size");
            if let Some(token) = &state.cancel_after_size {
                token.cancelled.store(true, Ordering::Relaxed);
            }
            match state.pane_size_after_send {
                Some(after) if state.sends > 0 => after,
                _ => state.pane_size,
            }
        }

        fn capture_ansi(&mut self, _session: &str, _scroll_back: i32) -> Option<String> {
            self.read("capture_ansi")
        }

        fn capture_bounded(&mut self, _s: &str, _b: i32, _timeout: Duration) -> Option<String> {
            self.read("capture_bounded")
        }

        fn pane_alive(&mut self, _session: &str) -> bool {
            let state = self.record("alive");
            !state.dead && state.dead_after_send.is_none_or(|at| state.sends <= at)
        }

        fn pane_pid(&mut self, _session: &str) -> Option<u32> {
            self.record("pane_pid").pane_pid
        }

        fn kill_tree(&mut self, _pid: u32, _identity: ProcessIdentity) -> bool {
            self.record("kill_tree");
            true
        }

        fn kill_session(&mut self, _session: &str, reason: &str) -> bool {
            self.record(&format!("kill_session:{reason}"));
            true
        }

        #[cfg(unix)]
        fn pane_stopped(&mut self, _s: &str, _pid: u32, _identity: ProcessIdentity) -> bool {
            self.record("stopped");
            true
        }
    }

    struct SpyGate(Rc<RefCell<SpyState>>);

    impl MutationGate for SpyGate {
        fn admit(&self, _session: &str) -> Result<(), InputRefusal> {
            match &mut self.0.borrow_mut().refuse_after {
                Some((0, refusal)) => Err(*refusal),
                Some((left, _)) => {
                    *left -= 1;
                    Ok(())
                }
                None => Ok(()),
            }
        }
    }

    pub(crate) fn resolved(host: TargetHost) -> InputTarget {
        InputTarget::from_session_target(&ResolvedSessionTarget {
            input: SessionTargetInput::RawName("AgentDesk-codex-p6b".to_string()),
            session_key: None,
            host,
        })
    }

    pub(crate) fn known(kind: HostKind) -> TargetHost {
        TargetHost::Known {
            kind,
            source: TargetSource::SessionRecord,
            name: "AgentDesk-codex-p6b".to_string(),
        }
    }

    /// Every resolved host that is not a confirmed tmux session.
    pub(crate) fn non_tmux_targets() -> Vec<(InputTarget, InputRefusal)> {
        let conflict = TargetHost::Conflict {
            first: (HostKind::Herdr, TargetSource::SessionRecord),
            second: (HostKind::Tmux, TargetSource::InflightLocator),
        };
        [
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
        ]
        .into_iter()
        .map(|(host, refusal)| (resolved(host), refusal))
        .collect()
    }

    /// Routes this thread's legacy Codex input through a spy until dropped.
    pub(crate) struct SpyGuard(pub Rc<RefCell<SpyState>>);

    impl SpyGuard {
        pub(crate) fn install(state: SpyState) -> Self {
            let state = Rc::new(RefCell::new(state));
            let injected: Injected = (
                Box::new(Spy(state.clone())),
                Box::new(SpyGate(state.clone())),
            );
            INJECTED.with(|slot| *slot.borrow_mut() = Some(injected));
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

    use super::spy::{Spy, SpyGuard, SpyState, exit, non_tmux_targets};
    use super::*;
    use crate::services::codex_tui::input::{
        CodexFollowupPromptSubmitOutcome, CodexPaneBusySignal, CodexPaneBusySignalTracker,
        submit_codex_followup_prompt,
    };

    const READY: &str = "\
╭──────────────────────────────────────────────────────────────╮
│ ▌                                                            │
╰──────────────────────────────────────────────────────────────╯
  Esc to interrupt   Ctrl+J newline   ⏎ send";

    fn draft_pane(draft: &str) -> String {
        format!(
            "╭──────────────────────────────────────────────╮\n\
             │ {draft} ▌                                  │\n\
             ╰──────────────────────────────────────────────╯\n  \
             Esc to interrupt   Ctrl+J newline   ⏎ send"
        )
    }

    fn state(captures: &[&str]) -> SpyState {
        SpyState {
            captures: captures.iter().map(|c| Some(c.to_string())).collect(),
            ..SpyState::default()
        }
    }

    fn calls(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn followup_submit_keeps_the_legacy_tmux_key_order() {
        let name = "p6b-spy-submit";
        let line = "한글 한 줄 후속 입력";
        let own_line = draft_pane(line);
        let guard = SpyGuard::install(state(&[READY, &own_line, READY]));
        let outcome = submit_codex_followup_prompt(name, line, None);
        assert!(matches!(
            outcome,
            CodexFollowupPromptSubmitOutcome::Submitted
        ));
        let literal = format!("literal:{line}");
        let submitted = [
            "capture_ansi",
            "alive",
            "pane_size",
            &literal,
            "pane_size",
            "alive",
            "capture_ansi",
            "keys:Enter",
            "capture_ansi",
            "alive",
        ];
        assert_eq!(guard.calls(), calls(&submitted));
        drop(guard);

        // A small multi-line Korean prompt goes load → paste → Enter through one buffer.
        let paste = "첫 줄 한글\n둘째 줄".to_string();
        let own_paste = format!(
            "› {}\n\n  gpt-5.4 · Fast off · Context 100% left",
            paste.replace('\n', "\n  ")
        );
        let guard = SpyGuard::install(state(&[READY, &own_paste, READY]));
        let outcome = submit_codex_followup_prompt(name, &paste, None);
        assert!(matches!(
            outcome,
            CodexFollowupPromptSubmitOutcome::Submitted
        ));
        let load = format!("load:{paste}");
        let pasted = [
            "capture_ansi",
            "alive",
            "pane_size",
            &load,
            "paste:delete=true",
            "pane_size",
            "alive",
            "capture_ansi",
            "keys:Enter",
            "capture_ansi",
            "alive",
        ];
        assert_eq!(guard.calls(), calls(&pasted));
        let buffers = guard.0.borrow().buffers.clone();
        assert_eq!(buffers.len(), 2);
        assert_eq!(buffers[0], buffers[1]);
        assert!(buffers[0].starts_with("agentdesk-codex-tui-input-"));
        drop(guard);

        // An oversized single line cannot prove visible ownership in an 80×24 pane.
        let long = "가".repeat(2000);
        let guard = SpyGuard::install(state(&[READY]));
        let outcome = submit_codex_followup_prompt(name, &long, None);
        assert!(
            matches!(
                outcome,
                CodexFollowupPromptSubmitOutcome::NotSubmitted { .. }
            ),
            "{outcome:?}"
        );
        assert_eq!(
            guard.calls(),
            calls(&["capture_ansi", "alive", "pane_size"])
        );
        assert_eq!(guard.0.borrow().sends, 0);
        let planned = crate::services::codex_tui::input::plan_prompt_submit(&long).unwrap();
        assert_eq!(
            planned,
            vec![
                TuiInputAction::Literal("가".repeat(1800)),
                TuiInputAction::Literal("가".repeat(200)),
                TuiInputAction::Enter,
            ]
        );
        drop(guard);

        // The busy signal reads the bounded capture of the same transport.
        let mut busy = state(&["• Working (5s • esc to interrupt)"]);
        busy.captures.push_back(None);
        let guard = SpyGuard::install(busy);
        let mut tracker = CodexPaneBusySignalTracker::default();
        assert_eq!(tracker.probe_tmux(name), CodexPaneBusySignal::Fresh);
        assert_eq!(tracker.probe_tmux(name), CodexPaneBusySignal::Unavailable);
        assert_eq!(
            guard.calls(),
            calls(&["capture_bounded", "capture_bounded"])
        );
    }

    #[test]
    fn a_send_that_may_have_landed_stops_and_is_never_resent() {
        let chunks = [
            TuiInputAction::Literal("첫".to_string()),
            TuiInputAction::Literal("둘".to_string()),
            TuiInputAction::Enter,
        ];
        let paste = [
            TuiInputAction::PasteBuffer("한글\n줄바꿈".to_string()),
            TuiInputAction::Enter,
        ];
        let literal = [
            TuiInputAction::Literal("x".to_string()),
            TuiInputAction::Enter,
        ];
        type Case<'a> = (
            &'a [TuiInputAction],
            usize,
            Result<Output, String>,
            usize,
            bool,
            &'a str,
        );
        let cases: [Case; 4] = [
            // ACK lost on the second chunk after the first landed.
            (
                &chunks,
                1,
                Err("ack lost".to_string()),
                1,
                false,
                "ack lost",
            ),
            // The buffer load fails before anything reaches the pane.
            (
                &paste,
                0,
                Err("load failed".to_string()),
                0,
                false,
                "load failed",
            ),
            (
                &paste,
                1,
                Ok(exit(1, "no buffer")),
                0,
                false,
                "tmux send paste-buffer failed: no buffer",
            ),
            // Enter itself fails after the literal landed.
            (
                &literal,
                1,
                Ok(exit(1, "no pane")),
                1,
                true,
                "tmux send enter failed: no pane",
            ),
        ];
        let target = InputTarget::legacy_tmux("p6b-spy-partial");
        for (plan, at, answer, confirmed, enter_attempted, error) in cases {
            let shared = std::rc::Rc::new(std::cell::RefCell::new(SpyState {
                fail_send: Some((at, answer)),
                ..SpyState::default()
            }));
            let stopped = run_plan(
                &target,
                &LegacyTmuxGate,
                &mut Spy(shared.clone()),
                plan,
                None,
            );
            let InputRun::Indeterminate { confirmed: got, .. } = &stopped.run else {
                panic!("{plan:?}: {stopped:?}");
            };
            assert_eq!(*got, confirmed, "{plan:?}");
            assert_eq!(stopped.enter_attempted, enter_attempted, "{plan:?}");
            assert_eq!(shared.borrow().sends, at + 1, "{plan:?}: nothing after it");
            assert!(!refused_by_gate(&stopped.run));
            assert_eq!(legacy_result(stopped.run), Err(error.to_string()));
        }

        // Production submit: a lost literal ACK is reported before Enter, with no Enter.
        let mut lost = state(&[READY]);
        lost.fail_send = Some((0, Err("ack lost".to_string())));
        let guard = SpyGuard::install(lost);
        let outcome = submit_codex_followup_prompt("p6b-spy-lost", "x", None);
        assert!(
            matches!(&outcome, CodexFollowupPromptSubmitOutcome::Refused {
                run: InputRun::Indeterminate {
                    confirmed: 0,
                    cause: StopCause::Send(error),
                },
            } if error == "ack lost"),
            "{outcome:?}"
        );
        assert_eq!(
            guard.calls(),
            calls(&["capture_ansi", "alive", "pane_size", "literal:x"])
        );
        drop(guard);

        // A load failure proves no pane effect, so the existing fallback remains legal.
        let mut load = state(&[READY]);
        load.fail_send = Some((0, Err("load failed".to_string())));
        let guard = SpyGuard::install(load);
        let outcome = submit_codex_followup_prompt("load-no-pane-effect", "first\nsecond", None);
        assert!(
            matches!(outcome, CodexFollowupPromptSubmitOutcome::NotSubmitted { error } if error == "load failed")
        );
        assert_eq!(
            guard.calls(),
            calls(&["capture_ansi", "alive", "pane_size", "load:first\nsecond"])
        );
        drop(guard);

        // A lost paste ACK cannot prove no effect even with zero confirmed writes.
        let mut paste = state(&[READY]);
        paste.fail_send = Some((1, Err("paste ack lost".to_string())));
        let guard = SpyGuard::install(paste);
        let outcome = submit_codex_followup_prompt("paste-may-have-landed", "first\nsecond", None);
        assert!(
            matches!(&outcome, CodexFollowupPromptSubmitOutcome::Refused {
            run: InputRun::Indeterminate { confirmed: 0, cause: StopCause::Send(error) },
        } if error == "paste ack lost")
        );
        assert_eq!(
            guard.calls(),
            calls(&[
                "capture_ansi",
                "alive",
                "pane_size",
                "load:first\nsecond",
                "paste:delete=true"
            ])
        );
        drop(guard);

        // A failed Enter is sent once and only confirmed by reading the pane.
        let own_x = draft_pane("x");
        let mut enter = state(&[READY, &own_x, READY]);
        enter.fail_send = Some((1, Ok(exit(1, "no pane"))));
        let guard = SpyGuard::install(enter);
        let outcome = submit_codex_followup_prompt("p6b-spy-enter", "x", None);
        assert!(
            matches!(&outcome, CodexFollowupPromptSubmitOutcome::Unconfirmed { error, .. } if error == "tmux send enter failed: no pane"),
            "{outcome:?}"
        );
        let expected = [
            "capture_ansi",
            "alive",
            "pane_size",
            "literal:x",
            "pane_size",
            "alive",
            "capture_ansi",
            "keys:Enter",
            "capture_ansi",
            "alive",
        ];
        assert_eq!(guard.calls(), calls(&expected));
    }

    #[test]
    fn a_gate_refusal_is_typed_and_nothing_follows() {
        let refusal = InputRefusal::IdentityMismatch;
        let guard = SpyGuard::install(SpyState {
            refuse_after: Some((0, refusal)),
            ..state(&[READY])
        });
        let outcome = submit_codex_followup_prompt("p6b-spy-gate", "x", None);
        assert!(
            matches!(&outcome, CodexFollowupPromptSubmitOutcome::Refused { run } if *run == InputRun::Refused(refusal)),
            "{outcome:?}"
        );
        assert_eq!(
            guard.calls(),
            calls(&["capture_ansi", "alive", "pane_size"])
        );
        drop(guard);

        // The host is swapped after the literal: no Enter, and the partial count is kept.
        let guard = SpyGuard::install(SpyState {
            refuse_after: Some((1, refusal)),
            ..state(&[READY])
        });
        let outcome = submit_codex_followup_prompt("p6b-spy-gate", "x", None);
        let swapped = InputRun::Indeterminate {
            confirmed: 1,
            cause: StopCause::Refused(refusal),
        };
        assert!(
            matches!(&outcome, CodexFollowupPromptSubmitOutcome::Refused { run } if *run == swapped),
            "{outcome:?}"
        );
        assert_eq!(
            guard.calls(),
            calls(&["capture_ansi", "alive", "pane_size", "literal:x"])
        );
        drop(guard);

        // A cancel after a partial write clears the matching draft with C-u, and
        // only after the gate admits that write too.
        let prompt = "hello world and more";
        let partial = draft_pane("hello world");
        for (refuse_after, cleared) in [(None, true), (Some((1, refusal)), false)] {
            let token = Arc::new(CancelToken::new());
            let guard = SpyGuard::install(SpyState {
                cancel_after_send: Some((0, token.clone())),
                refuse_after,
                ..state(&[READY, &partial])
            });
            let outcome = submit_codex_followup_prompt("p6b-spy-cancel", prompt, Some(&token));
            assert!(matches!(
                outcome,
                CodexFollowupPromptSubmitOutcome::Cancelled
            ));
            let mut expected = vec![
                "capture_ansi",
                "alive",
                "pane_size",
                "literal:hello world and more",
                "capture_ansi",
                "alive",
            ];
            expected.extend(cleared.then_some("keys:C-u"));
            assert_eq!(guard.calls(), calls(&expected), "{refuse_after:?}");
        }
    }

    #[test]
    fn foldable_multiline_host_plan_is_a_typed_preflight_refusal() {
        use crate::services::tui_input::submission::Refusal;

        let prompt = format!("{}\ny", "x".repeat(1000));
        let plan = [TuiInputAction::PasteBuffer(prompt), TuiInputAction::Enter];
        let guard = SpyGuard::install(SpyState {
            pane_size: Some((2000, 24)),
            ..SpyState::default()
        });
        let result =
            run_prompt_submission_legacy("host-folded-preflight-fixture", &plan, Some(READY), None);
        assert_eq!(
            result.run,
            InputRun::Refused(InputRefusal::Composer(Refusal::UnpredictableRender))
        );
        assert!(!result.composer_mutated);
        assert!(!result.enter_attempted);
        assert_eq!(guard.calls(), calls(&["pane_size"]));
        assert_eq!(guard.0.borrow().sends, 0);
    }

    #[test]
    fn host_guard_observes_liveness_before_the_last_capture_and_enter() {
        let own = draft_pane("hello");
        let guard = SpyGuard::install(state(&[READY, &own, READY]));
        let outcome = submit_codex_followup_prompt("host-last-read-order-fixture", "hello", None);
        assert!(
            matches!(outcome, CodexFollowupPromptSubmitOutcome::Submitted),
            "{outcome:?}"
        );
        assert_eq!(
            guard.calls(),
            calls(&[
                "capture_ansi",
                "alive",
                "pane_size",
                "literal:hello",
                "pane_size",
                "alive",
                "capture_ansi",
                "keys:Enter",
                "capture_ansi",
                "alive",
            ])
        );
        drop(guard);

        let guard = SpyGuard::install(SpyState {
            dead_after_send: Some(0),
            ..state(&[READY, &own])
        });
        let outcome =
            submit_codex_followup_prompt("host-dead-before-last-capture-fixture", "hello", None);
        assert!(
            matches!(outcome, CodexFollowupPromptSubmitOutcome::Refused { .. }),
            "{outcome:?}"
        );
        assert_eq!(
            guard.calls(),
            calls(&[
                "capture_ansi",
                "alive",
                "pane_size",
                "literal:hello",
                "pane_size",
                "alive",
            ])
        );
        assert_eq!(
            guard.0.borrow().captures.len(),
            1,
            "dead pane was captured after liveness failed"
        );
    }

    #[test]
    fn host_preflight_preserves_prior_guard_validation_and_cancellation() {
        use crate::services::tui_input::submission::Refusal;
        use std::sync::atomic::Ordering;

        let plan = [
            TuiInputAction::Literal("hello".into()),
            TuiInputAction::Enter,
        ];
        let foreign = draft_pane("a person's draft");
        let modal = format!("Approval required\n1. Yes\n2. No\n{READY}");
        for before in [&foreign, &modal] {
            let guard = SpyGuard::install(SpyState {
                pane_size: None,
                ..SpyState::default()
            });
            let result =
                run_prompt_submission_legacy("host-prior-guard-fixture", &plan, Some(before), None);
            assert_eq!(
                result.run,
                InputRun::Refused(InputRefusal::Composer(Refusal::NotReady))
            );
            assert!(!result.composer_mutated);
            assert!(!result.enter_attempted);
            assert!(
                guard.calls().is_empty(),
                "first guard consulted capacity: {:?}",
                guard.calls()
            );
            assert_eq!(guard.0.borrow().sends, 0);
        }

        let invalid = [
            TuiInputAction::Literal("hello\u{1b}".into()),
            TuiInputAction::Enter,
        ];
        let guard = SpyGuard::install(SpyState {
            pane_size: None,
            ..SpyState::default()
        });
        let result = run_prompt_submission_legacy(
            "host-prior-validation-fixture",
            &invalid,
            Some(READY),
            None,
        );
        assert_eq!(
            result.run,
            InputRun::Refused(InputRefusal::Composer(Refusal::InvalidPrompt))
        );
        assert!(!result.composer_mutated);
        assert!(!result.enter_attempted);
        assert!(
            guard.calls().is_empty(),
            "invalid prompt consulted capacity: {:?}",
            guard.calls()
        );
        assert_eq!(guard.0.borrow().sends, 0);
        drop(guard);

        let token = CancelToken::new();
        token.cancelled.store(true, Ordering::Relaxed);
        let guard = SpyGuard::install(SpyState {
            pane_size: None,
            ..SpyState::default()
        });
        let result = run_prompt_submission_legacy(
            "host-prior-cancellation-fixture",
            &plan,
            Some(READY),
            Some(&token),
        );
        assert_eq!(result.run, InputRun::Cancelled { confirmed: 0 });
        assert!(!result.composer_mutated);
        assert!(!result.enter_attempted);
        assert!(
            guard.calls().is_empty(),
            "cancelled prompt consulted capacity: {:?}",
            guard.calls()
        );
        assert_eq!(guard.0.borrow().sends, 0);
    }

    #[test]
    fn non_tmux_targets_get_no_keys_and_no_kill() {
        let plan = [
            TuiInputAction::Literal("x".to_string()),
            TuiInputAction::Enter,
        ];
        for (target, refusal) in non_tmux_targets() {
            assert_eq!(target, InputTarget::Refused(refusal));
            let shared = std::rc::Rc::new(std::cell::RefCell::new(SpyState::default()));
            let run = run_plan(
                &target,
                &LegacyTmuxGate,
                &mut Spy(shared.clone()),
                &plan,
                None,
            );
            assert_eq!(run.run, InputRun::Refused(refusal));
            assert!(shared.borrow().calls.is_empty(), "{refusal:?}: no key");

            #[cfg(unix)]
            {
                let guard = SpyGuard::install(SpyState {
                    pane_pid: Some(4242),
                    ..SpyState::default()
                });
                assert!(kill_legacy_pane(&target, "p6b").is_err());
                assert!(
                    guard.calls().is_empty(),
                    "{refusal:?}: no pane_pid, no kill"
                );
            }
        }

        // Positive control: a confirmed tmux session keeps the legacy kill order.
        #[cfg(unix)]
        {
            use super::spy::{known, resolved};
            use crate::services::session_host::HostKind;

            let tmux = resolved(known(HostKind::Tmux));
            assert_eq!(tmux, InputTarget::Tmux("AgentDesk-codex-p6b".to_string()));
            let guard = SpyGuard::install(SpyState {
                pane_pid: Some(4242),
                ..SpyState::default()
            });
            assert_eq!(kill_legacy_pane(&tmux, "stale draft"), Ok(()));
            let order = [
                "pane_pid",
                "kill_tree",
                "kill_session:stale draft",
                "stopped",
            ];
            assert_eq!(guard.calls(), calls(&order));
            drop(guard);

            let guard = SpyGuard::install(SpyState::default());
            assert_eq!(
                kill_legacy_pane(&tmux, "stale draft"),
                Err("Codex TUI warm follow-up could not pin the pane PID before kill".to_string())
            );
            assert_eq!(guard.calls(), calls(&["pane_pid"]));
        }
    }
}
