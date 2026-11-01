//! The gated content write returns before Source and provider completion observation.
use super::*;

pub(super) struct Submitted {
    attached: Attached,
    transcript: String,
    before: u64,
    started_at: chrono::DateTime<chrono::Utc>,
}

pub(super) fn submit(
    turn: &HerdrTurn<'_>,
    attached: Attached,
    sender: &Sender<StreamMessage>,
) -> Result<Option<Submitted>, String> {
    let path = attached.transcript.as_path();
    let transcript = path.display().to_string();
    let length = || std::fs::metadata(path).map_or(0, |meta| meta.len());
    let session_id = attached.session_id.clone();
    let _ = sender.send(StreamMessage::Init {
        session_id: session_id.clone(),
        raw_session_id: Some(session_id),
    });
    let (before, started_at) = (length(), chrono::Utc::now());
    let plan = [
        TuiInputAction::PasteBuffer(turn.prompt.to_owned()),
        TuiInputAction::Enter,
    ];
    let cancel = turn.cancel.as_deref();
    let held = hold(&attached.nonce)?;
    #[cfg(test)]
    before_start::test_input_boundary(cancel);
    let start = || {
        let mut start = HerdrTurnStart::at_end_of(&attached.nonce, path, Some(started_at));
        start.offset = before;
        start
    };
    let HerdrInputRun::Ran(run) = observed_input(cancel, start, || {
        composer::run(&attached.target, &plan, cancel)
    })?
    else {
        if !before_start::mutant("closed_release_callsite_removed_claude") {
            before_start::release_claude_hold(&turn.owner.logical_key, &held);
        }
        return Ok(None);
    };
    crate::services::tui_o::exact_submission::observe_untouched(matches!(
        run,
        InputRun::Refused(_) | InputRun::Cancelled { confirmed: 0 }
    ));
    if composer_settled(&run)
        && let Err(error) = std::fs::remove_file(&held)
    {
        tracing::warn!(logical = %turn.owner.logical_key, %error, "herdr turn: input hold kept");
    }
    run.into_legacy()?;
    Ok(Some(Submitted {
        attached,
        transcript,
        before,
        started_at,
    }))
}

pub(super) fn observe(
    turn: &HerdrTurn<'_>,
    runtime: &Handle,
    submitted: Submitted,
    sender: Sender<StreamMessage>,
) -> Result<(), String> {
    let Submitted {
        attached,
        transcript,
        before,
        started_at,
    } = submitted;
    let path = attached.transcript.as_path();
    let length = || std::fs::metadata(path).map_or(0, |meta| meta.len());
    let cancel = turn.cancel.as_deref();
    let start = before;
    let logical = turn.owner.logical_key.as_str();
    let unread = || PromptReadinessSnapshot {
        prompt_marker_detected: false,
        prompt_draft_detected: false,
        tmux_pane_alive: false,
        capture_available: false,
        pane_tail: String::new(),
    };
    let early = super::super::wait_for_claude_tui_transcript_file_inner(
        &transcript,
        start,
        cancel,
        logical,
        super::super::CLAUDE_TUI_TRANSCRIPT_INITIAL_WAIT_TIMEOUT,
        || path.exists(),
        || true,
        unread,
    )?;
    let (mut terminal, mut unconfirmed) = (None, false);
    if early.is_none() {
        let kind = Some(RuntimeHandoffKind::ClaudeTui);
        let probe =
            super::super::host_gate::host_poll_probe(None, ProviderKind::Claude, kind, &transcript);
        let cancel = turn.cancel.clone();
        if let Some(token) = cancel.as_ref()
            && herdr_provider_terminal_only(Some(token)).is_some()
        {
            let ended =
                |at| terminal_replay::ended_tail(&transcript, at, started_at, &attached.target);
            let read = read_to_provider_terminal(&transcript, start, &sender, token, ended);
            terminal = read?.map(|terminal| (terminal, token.clone()));
            unconfirmed = terminal.is_none();
        } else {
            read_output_file_until_result_with_harvest(
                &transcript,
                start,
                sender.clone(),
                cancel,
                probe,
            )
            .map_err(|failure| failure.error)?;
        }
    }
    // A read the row pass woke after settling the turn elsewhere binds nothing to its execution.
    let woken = turn
        .cancel
        .as_deref()
        .is_some_and(CancelToken::is_completion_cleanup);
    if unconfirmed && woken && !before_start::mutant("prb_woken_read_binds") {
        return Ok(());
    }
    // A Herdr turn hands off exactly where its terminal record ends, never past it.
    let terminal_end = terminal.as_ref().map(|(read, ..)| read.end);
    let handoff_end = || terminal_end.unwrap_or_else(length);
    // The tmux handoff's launch registration; with the transcript now written it resolves a cold
    // start's Pending source, before any terminal of this turn reaches admission.
    let binding = TuiRuntimeBinding {
        runtime_kind: RuntimeHandoffKind::ClaudeTui,
        output_path: transcript.clone(),
        relay_output_path: None,
        input_fifo_path: None,
        session_id: Some(attached.session_id.clone()),
        last_offset: handoff_end(),
        relay_last_offset: None,
    };
    crate::services::tui_prompt_dedupe::register_launched_tmux_runtime_binding(logical, binding);
    if !attached.bound {
        runtime.block_on(bind_once_logged(turn, &attached.nonce));
    }
    if let Some((read, token)) = terminal.take() {
        let _ = sender.send(read.frame(&transcript, logical, &token));
    }
    // A read that ended without this turn's terminal hands no runtime to a watcher to finish.
    if unconfirmed {
        return Ok(());
    }
    let _ = sender.send(StreamMessage::RuntimeReady {
        handoff: RuntimeHandoff::ClaudeTui {
            transcript_path: transcript,
            tmux_session_name: logical.to_owned(),
            last_offset: handoff_end(),
            turn_read: None,
        },
    });
    Ok(())
}

#[cfg(test)]
mod tests;
