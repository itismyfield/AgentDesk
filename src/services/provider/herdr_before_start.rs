//! Input lifetime evidence, separate from the provider's submission observation.
use super::CancelToken;
use super::cancel_token_claude_interrupt::{HerdrInterruptState, HerdrSubmission, OwnStart};
use std::sync::atomic::Ordering;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InputPhase {
    BeforeInput,
    Attempting,
    FinishedNoAttempt,
    FinishedUntouched,
    MayHaveWritten,
    Closed,
}

pub(crate) struct HerdrInputState {
    pub(crate) submission: HerdrSubmission,
    pub(crate) phase: InputPhase,
    #[cfg(test)]
    pub(crate) exit: Option<ExitDecision>,
}

impl Default for HerdrInputState {
    fn default() -> Self {
        Self {
            submission: HerdrSubmission::Unsubmitted,
            phase: InputPhase::BeforeInput,
            #[cfg(test)]
            exit: None,
        }
    }
}

/// A Closed result never invokes the host input runner.
pub(crate) enum InputRun<T> {
    Ran(T),
    Closed,
}

pub(crate) fn mutant(name: &str) -> bool {
    #[cfg(test)]
    return std::env::var("ADK_COLDSTOP_MUTANT").ok().as_deref() == Some(name);
    #[cfg(not(test))]
    {
        let _ = name;
        false
    }
}

impl HerdrInterruptState {
    /// Caller holds the input lock. Own-start is only negative evidence.
    pub(crate) fn close_before_start(&self, input: &mut HerdrInputState) -> bool {
        self.close_observed(input, self.user_stop.load(Ordering::Acquire))
    }

    /// Closes on one stop observation `stop`, so a caller deciding more from it sees one instant.
    fn close_observed(&self, input: &mut HerdrInputState, stop: bool) -> bool {
        if !super::cancel_token_claude_interrupt::herdr_stop_settlement_available()
            && !mutant("policy_settlement_gate_removed")
        {
            return false;
        }
        if input.phase == InputPhase::Closed {
            return input.submission == HerdrSubmission::Unsubmitted;
        }
        if !stop
            || input.submission != HerdrSubmission::Unsubmitted
            || !matches!(
                input.phase,
                InputPhase::BeforeInput
                    | InputPhase::FinishedNoAttempt
                    | InputPhase::FinishedUntouched
            )
        {
            return false;
        }
        let mut own = self.own_start.lock().unwrap_or_else(|e| e.into_inner());
        match &mut *own {
            OwnStart::Unseen(callback) => {
                callback.take();
            }
            OwnStart::Seen { .. } => return false,
        }
        input.phase = InputPhase::Closed;
        true
    }

    /// Nonblocking diagnostic: contention is not proof that nothing was written.
    pub(crate) fn closed_probe(&self) -> Option<bool> {
        self.submission.try_lock().ok().map(|input| {
            input.phase == InputPhase::Closed && input.submission == HerdrSubmission::Unsubmitted
        })
    }

    pub(crate) fn prepare_input(&self, input: &mut HerdrInputState) -> bool {
        if !mutant("cold_preinput_check_removed") && self.close_before_start(input) {
            return mutant("close_does_not_block_input");
        }
        if input.phase == InputPhase::Closed {
            return mutant("close_does_not_block_input");
        }
        input.phase = InputPhase::Attempting;
        true
    }

    pub(crate) fn finish_input(
        &self,
        input: &mut HerdrInputState,
        submission: HerdrSubmission,
        untouched: bool,
    ) {
        input.submission = submission;
        input.phase = if (untouched && !mutant("typed_untouched_ignored"))
            || mutant("indeterminate_is_untouched")
        {
            InputPhase::FinishedUntouched
        } else {
            InputPhase::MayHaveWritten
        };
    }
}

/// Runs only on the blocking executor thread, after all Result exits converge.
pub(crate) fn finish_execution(token: Option<&CancelToken>) {
    if !super::cancel_token_claude_interrupt::herdr_stop_settlement_available()
        && crate::services::tui_o::exact_submission::logical_key().is_none()
    {
        return;
    }
    let Some(state) = token.and_then(CancelToken::herdr_interrupt_state) else {
        return;
    };
    let mut input = state.submission.lock().unwrap_or_else(|e| e.into_inner());
    if input.phase == InputPhase::BeforeInput && !mutant("noattempt_error_exit_unclosed") {
        input.phase = InputPhase::FinishedNoAttempt;
    }
    state.close_before_start(&mut input);
}

/// Runs one execution after its prelaunch check and settles its input phase whatever it returns.
pub(crate) fn observed_execution(
    token: Option<&CancelToken>,
    run: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    if prelaunch_closed(token)? {
        return Ok(());
    }
    let result = run();
    finish_execution(token);
    result
}

/// Releases a Claude input hold whose turn closed before any input.
pub(crate) fn release_claude_hold(logical: &str, held: &std::path::Path) {
    if !mutant("closed_keeps_input_hold")
        && let Err(error) = std::fs::remove_file(held)
    {
        tracing::warn!(logical, %error, "herdr turn: input hold kept");
    }
}

pub(crate) fn prelaunch_closed(token: Option<&CancelToken>) -> Result<bool, String> {
    if !super::cancel_token_claude_interrupt::herdr_stop_settlement_available()
        && !mutant("policy_settlement_gate_removed")
    {
        return Ok(false);
    }
    let Some(state) = token.and_then(CancelToken::herdr_interrupt_state) else {
        return Ok(false);
    };
    let mut input = state
        .submission
        .try_lock()
        .map_err(|_| "herdr turn: input observation busy; turn stays held".to_owned())?;
    Ok(!mutant("cold_preinput_check_removed") && state.close_before_start(&mut input))
}

#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BeforeStartProof {
    pub(crate) owner: crate::db::dispatched_sessions::hosted_execution::HostedOwner,
    pub(crate) turn_nonce: String,
    pub(crate) generation: u64,
}

#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ExitDecision {
    Normal,
    Hold,
    PolicyClose(BeforeStartProof),
}

#[cfg(test)]
thread_local! {
    /// Runs inside a seal after its one stop observation, where a concurrent stop can land.
    pub(crate) static EXIT_OBSERVED: std::cell::Cell<Option<fn(&HerdrInterruptState)>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(crate) fn seal_exit(
    token: &CancelToken,
    cancelled: bool,
    terminal_admitted: bool,
) -> ExitDecision {
    use super::cancel_token_claude_interrupt::herdr_stop_settlement_available;
    let exception = !herdr_stop_settlement_available() || cancelled || terminal_admitted;
    if exception && !mutant("exit_exception_after_try_lock") {
        return ExitDecision::Normal;
    }
    let Some(state) = token.herdr_interrupt_state() else {
        return ExitDecision::Normal;
    };
    let Ok(mut input) = state.submission.try_lock() else {
        return ExitDecision::Hold;
    };
    if let Some(sealed) = &input.exit {
        return sealed.clone();
    }
    // One stop observation decides both the close and the hold: a stop taken after it is late.
    let stop = state.user_stop.load(Ordering::Acquire);
    let closed = !exception
        && matches!(
            input.phase,
            InputPhase::FinishedNoAttempt | InputPhase::FinishedUntouched | InputPhase::Closed
        )
        && state.close_observed(&mut input, stop)
        && token.turn_nonce().is_some();
    if let Some(hook) = EXIT_OBSERVED.with(std::cell::Cell::get) {
        hook(&state);
    }
    let stop = match mutant("exit_stop_reread") {
        true => state.user_stop.load(Ordering::Acquire),
        false => stop,
    };
    let decision = if exception {
        ExitDecision::Normal
    } else if closed {
        ExitDecision::PolicyClose(BeforeStartProof {
            owner: state.owner.clone(),
            turn_nonce: token.turn_nonce().unwrap().to_owned(),
            generation: token.claude_interrupt_generation(),
        })
    } else if stop || input.submission != HerdrSubmission::Unsubmitted {
        ExitDecision::Hold
    } else {
        ExitDecision::Normal
    };
    input.exit = Some(decision.clone());
    decision
}

pub(crate) fn claude_observed_input(
    cancel: Option<&super::CancelToken>,
    start: impl FnOnce() -> crate::services::provider::cancel_token_claude_interrupt::HerdrTurnStart,
    write: impl FnOnce() -> crate::services::claude_tui::host_input::InputRun,
) -> Result<
    crate::services::provider::herdr_before_start::InputRun<
        crate::services::claude_tui::host_input::InputRun,
    >,
    String,
> {
    use crate::services::claude_tui::host_input::InputRun;
    use crate::services::provider::cancel_token_claude_interrupt::HerdrSubmission;
    use crate::services::provider::herdr_before_start::InputRun as HerdrInputRun;
    let state = cancel
        .filter(|_| {
            super::cancel_token_claude_interrupt::herdr_stop_settlement_available()
                || crate::services::tui_o::exact_submission::logical_key().is_some()
        })
        .and_then(CancelToken::herdr_interrupt_state);
    let run = if let Some(state) = state {
        let mut input = state.submission.lock().unwrap_or_else(|e| e.into_inner());
        if !state.prepare_input(&mut input) {
            return Ok(HerdrInputRun::Closed);
        }
        crate::services::tui_o::exact_submission::begin_input()?;
        if !state.record_turn_start(start()) {
            return Err("herdr turn: the token already began another turn".into());
        }
        let run = write();
        let observation = match &run {
            InputRun::Applied => HerdrSubmission::Submitted,
            InputRun::Indeterminate {
                confirmed: 1,
                cause: crate::services::claude_tui::host_input::StopCause::Send(_),
            } => HerdrSubmission::Unknown,
            _ => HerdrSubmission::Unsubmitted,
        };
        state.finish_input(
            &mut input,
            observation,
            matches!(
                run,
                InputRun::Refused(_) | InputRun::Cancelled { confirmed: 0 }
            ),
        );
        run
    } else {
        crate::services::tui_o::exact_submission::begin_input()?;
        write()
    };
    Ok(HerdrInputRun::Ran(run))
}

#[cfg(test)]
static BOUNDARY_STOPS: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<String>>> =
    std::sync::LazyLock::new(Default::default);
#[cfg(test)]
pub(crate) fn stop_at_input_boundary(logical: &str, stop: bool) {
    let mut stops = BOUNDARY_STOPS.lock().unwrap();
    if stop {
        stops.insert(logical.into());
    } else {
        stops.remove(logical);
    }
}
#[cfg(test)]
pub(crate) fn test_input_boundary(token: Option<&CancelToken>) {
    if let Some(state) = token.and_then(CancelToken::herdr_interrupt_state)
        && BOUNDARY_STOPS
            .lock()
            .unwrap()
            .contains(&state.owner.logical_key)
    {
        state.user_stop.store(true, Ordering::Release);
    }
}
