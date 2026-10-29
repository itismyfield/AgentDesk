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
    let (logical, cancel) = (turn.owner.logical_key.as_str(), turn.cancel.as_deref());
    let at = (logical, turn.channel_id, nonce);
    write(at, cancel, turn.prompt, || {
        gate(&current_record(turn, runtime, nonce)?)
    })
}

/// The first prompt through the owner waiting on this thread: it reserves the frame on the
/// execution the launch recorded, binds its pane, and runs this write as its effect.
pub(super) fn submit_owned(
    turn: &CodexHerdrTurn<'_>,
    hook: crate::services::claude_tui::owner_input::LaunchHook,
    lineage: crate::services::claude_tui::owner_input::Lineage,
    record: &HostedExecution,
) -> Result<Submitted, String> {
    let nonce = record.execution_nonce.as_str();
    let location = record
        .location
        .as_ref()
        .ok_or("herdr turn: the execution has no pane")?;
    let (socket, pane) = (location.socket_addr.clone(), location.pane_id.clone());
    let target = gate(record)?;
    let logical = turn.owner.logical_key.clone();
    let attempt = serde_json::json!({"host":"herdr","owner":logical,"root":socket,
        "execution_nonce":nonce});
    let source = serde_json::json!({"kind":"pre_source","expected_session_id":null,
        "expected_transcript":null,"binding_watermark":null});
    let slot = Arc::new(std::sync::Mutex::new(None));
    let (ticket, channel, cancel, nonce) = (
        slot.clone(),
        turn.channel_id,
        turn.cancel.clone(),
        nonce.to_owned(),
    );
    let submit = Box::new(move |frame: String| {
        let at = (logical.as_str(), channel, nonce.as_str());
        let written = write(at, cancel.as_deref(), &frame, || Ok(target))?;
        let closed = || "herdr turn: the input closed before its first key".to_string();
        let submitted = written.ok_or_else(closed)?;
        *ticket.lock().unwrap_or_else(|e| e.into_inner()) = Some(submitted);
        Ok(())
    });
    hook(
        lineage,
        attempt,
        source,
        Some(format!("{socket}#{pane}")),
        submit,
    )?;
    let submitted = slot.lock().unwrap_or_else(|e| e.into_inner()).take();
    submitted.ok_or_else(|| "herdr turn: the owner submitted without a ticket".into())
}

fn write(
    (logical, channel, nonce): (&str, u64, &str),
    cancel: Option<&CancelToken>,
    prompt: &str,
    target: impl FnOnce() -> Result<HerdrTarget, String>,
) -> Result<Option<Submitted>, String> {
    let plan = plan_prompt_submit(prompt)?;
    // Opened before the prompt, so a fast hook cannot land unseen.
    let logged = subscribe_binding_events(channel)
        .map_err(|error| format!("herdr turn: binding log unavailable: {error}"))?;
    let target = target()?;
    empty_composer(&target, cancel)?;
    crate::services::tui_prompt_dedupe::register_provider_session("codex", logical, logical);
    crate::services::tui_prompt_dedupe::register_codex_herdr_placeholder(logical, channel);
    hold(nonce)?;
    #[cfg(test)]
    crate::services::provider::herdr_before_start::test_input_boundary(cancel);
    let run = observed_input(cancel, nonce, || None, || run_herdr(&target, &plan, cancel))?;
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
