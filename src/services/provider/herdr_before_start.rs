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
        if !super::cancel_token_claude_interrupt::herdr_stop_settlement_available()
            && !mutant("policy_settlement_gate_removed")
        {
            return false;
        }
        if input.phase == InputPhase::Closed {
            return input.submission == HerdrSubmission::Unsubmitted;
        }
        if !self.user_stop.load(Ordering::Acquire)
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
pub(crate) fn seal_exit(
    token: &CancelToken,
    cancelled: bool,
    terminal_admitted: bool,
) -> ExitDecision {
    use super::cancel_token_claude_interrupt::herdr_stop_settlement_available;
    let Some(state) = token.herdr_interrupt_state() else {
        return ExitDecision::Normal;
    };
    let Ok(mut input) = state.submission.try_lock() else {
        return ExitDecision::Hold;
    };
    if let Some(sealed) = &input.exit {
        return sealed.clone();
    }
    let decision = if !herdr_stop_settlement_available() || cancelled || terminal_admitted {
        ExitDecision::Normal
    } else if matches!(
        input.phase,
        InputPhase::FinishedNoAttempt | InputPhase::FinishedUntouched | InputPhase::Closed
    ) && state.close_before_start(&mut input)
        && token.turn_nonce().is_some()
    {
        ExitDecision::PolicyClose(BeforeStartProof {
            owner: state.owner.clone(),
            turn_nonce: token.turn_nonce().unwrap().to_owned(),
            generation: token.claude_interrupt_generation(),
        })
    } else if state.user_stop.load(Ordering::Acquire)
        || input.submission != HerdrSubmission::Unsubmitted
    {
        ExitDecision::Hold
    } else {
        ExitDecision::Normal
    };
    input.exit = Some(decision.clone());
    decision
}
