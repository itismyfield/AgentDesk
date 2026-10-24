//! Submit owns the physical mutation; Source binding and the reply belong to observation.
use super::*;

pub(super) struct Submitted {
    target: HerdrTarget,
    logged: tokio::sync::watch::Receiver<u64>,
}

pub(super) fn submit(
    turn: &CodexHerdrTurn<'_>,
    runtime: &Handle,
    nonce: &str,
) -> Result<Option<Submitted>, String> {
    let logical = turn.owner.logical_key.as_str();
    let plan = plan_prompt_submit(turn.prompt)?;
    // Opened before the prompt, so a fast hook cannot land unseen.
    let logged = subscribe_binding_events(turn.channel_id)
        .map_err(|error| format!("herdr turn: binding log unavailable: {error}"))?;
    let target = gate(&current_record(turn, runtime, nonce)?)?;
    empty_composer(&target, turn.cancel.as_deref())?;
    crate::services::tui_prompt_dedupe::register_provider_session("codex", logical, logical);
    crate::services::tui_prompt_dedupe::register_codex_herdr_placeholder(logical, turn.channel_id);
    hold(nonce)?;
    #[cfg(test)]
    crate::services::provider::herdr_before_start::test_input_boundary(turn.cancel.as_deref());
    let run = observed_input(
        turn.cancel.as_deref(),
        nonce,
        || None,
        || run_herdr(&target, &plan, turn.cancel.as_deref()),
    )?;
    let run = match run {
        crate::services::provider::herdr_before_start::InputRun::Ran(run) => run,
        crate::services::provider::herdr_before_start::InputRun::Closed => {
            if !crate::services::provider::herdr_before_start::mutant(
                "closed_release_callsite_removed_codex_first",
            ) {
                release_closed_hold(logical, nonce);
            }
            return Ok(None);
        }
    };
    if run.run != InputRun::Applied {
        if composer_untouched(&run) {
            warn_release(logical, release_hold(nonce));
        }
        legacy_result(run.run)?;
        return Ok(None);
    }
    Ok(Some(Submitted { target, logged }))
}

pub(super) fn observe(
    turn: &CodexHerdrTurn<'_>,
    runtime: &Handle,
    ports: &dyn CodexHerdrPorts,
    nonce: &str,
    sender: Sender<StreamMessage>,
    submitted: Submitted,
) -> Result<(), String> {
    let logical = turn.owner.logical_key.as_str();
    let Submitted { target, mut logged } = submitted;
    // Submitted: a cancel from here on is remembered on its token, never a reason to stop waiting.
    let deadline = Instant::now() + SOURCE_WAIT;
    let source = loop {
        if let Some(source) = logged_source(turn, nonce) {
            break source;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(format!(
                "herdr turn: no session start of codex execution {nonce}; later prompts stay held"
            ));
        }
        let poll = left.min(Duration::from_millis(200));
        let _ = runtime.block_on(tokio::time::timeout(poll, logged.changed()));
    };
    let record = current_record(turn, runtime, nonce)?;
    if !ports.attach(&turn.owner, &record, &source, &target)? {
        return Err(format!("herdr turn: codex execution {nonce} is not bound"));
    }
    // The launch's own new rollout begins with this prompt's turn.
    let start = HerdrTurnStart {
        execution_nonce: nonce.to_owned(),
        source: source.path.clone(),
        file: Some((source.dev, source.ino)),
        offset: 0,
        submitted_at: None,
    };
    record_turn_start(turn.cancel.as_deref(), start)?;
    warn_release(logical, release_hold(nonce));
    read_reply(
        turn,
        &target,
        &source.path,
        0,
        source.session_id.clone(),
        sender,
    )
}

#[cfg(test)]
mod pg_tests;
