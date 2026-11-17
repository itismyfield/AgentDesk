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
    write(
        at,
        cancel,
        turn.prompt,
        || gate(&current_record(turn, runtime, nonce)?),
        || restore_pending(turn.pool, &turn.owner, runtime, nonce),
    )
    .map_err(SubmissionError::into_legacy)
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
    let (pool, owner, runtime) = (turn.pool.clone(), turn.owner.clone(), Handle::current());
    let epoch_scope = clear_epoch::current();
    let submit = Box::new(move |frame: String| {
        let _epoch = clear_epoch::install(epoch_scope);
        let at = (logical.as_str(), channel, nonce.as_str());
        let written = write(
            at,
            cancel.as_deref(),
            &frame,
            || Ok(target),
            || restore_pending(&pool, &owner, &runtime, &nonce),
        )?;
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

fn restore_pending(
    pool: &PgPool,
    owner: &HostedOwner,
    runtime: &Handle,
    nonce: &str,
) -> Result<(), String> {
    use crate::services::tmux_common as tc;
    use crate::services::tui_prompt_dedupe::{self as dedupe, binding_context as context};
    let order = dedupe::herdr_listing_order(&owner.logical_key);
    let order_guard = order.lock().unwrap_or_else(|p| p.into_inner());
    let restored = tc::with_tmux_source_authority(&owner.logical_key, |authority| {
        let HostedLookup::Found(found) = runtime.block_on(load(pool, owner)) else {
            return Err("herdr turn: Pending row unreadable before input".into());
        };
        let HostedRecord::Known(record) = found.record else {
            return Err("herdr turn: no Pending execution before input".into());
        };
        if record.state != HostedState::Pending
            || record.owner != *owner
            || record.execution_nonce != nonce
            || context::observe_spawn_nonce_marker(authority.session())
                != context::SpawnNonceMarker::Known(nonce.to_owned())
        {
            return Err("herdr turn: Pending execution changed before input".into());
        }
        if !dedupe::restore_herdr_execution_under_source_authority(authority, nonce) {
            return Err("herdr turn: another execution is registered before input".into());
        }
        Ok(())
    });
    drop(order_guard);
    restored
}

fn write(
    (logical, channel, nonce): (&str, u64, &str),
    cancel: Option<&CancelToken>,
    prompt: &str,
    target: impl FnOnce() -> Result<HerdrTarget, String>,
    restore: impl FnOnce() -> Result<(), String>,
) -> Result<Option<Submitted>, SubmissionError> {
    let epoch_scope = clear_epoch::current();
    let checked = epoch_scope
        .as_ref()
        .map(|scope| scope.check_epoch())
        .transpose()
        .map_err(|_| owned_hold("clear_epoch_changed"))?;
    let owned = crate::services::claude_tui::owner_input::active();
    let plan = if owned {
        crate::services::codex_tui::input::owned_prompt_plan(prompt)?
    } else {
        plan_prompt_submit(prompt)?
    };
    // Opened before the prompt, so a fast hook cannot land unseen.
    let logged = subscribe_binding_events(channel).map_err(|error| {
        if owned {
            crate::services::claude_tui::owner_input::vetoed("binding_log_unavailable");
        }
        format!("herdr turn: binding log unavailable: {error}")
    })?;
    drop(
        crate::services::tui_prompt_dedupe::binding_context::herdr_resume::pending_start(nonce)
            .inspect_err(|_| {
                if owned {
                    crate::services::claude_tui::owner_input::vetoed("resume_start_unavailable");
                }
            })?,
    );
    let target = target()?;
    let draft = if owned {
        owned_composer(&target, cancel)?
    } else {
        empty_composer(&target, cancel)?;
        false
    };
    restore().inspect_err(|_| {
        if owned {
            crate::services::claude_tui::owner_input::vetoed("pending_restore_refused");
        }
    })?;
    crate::services::tui_prompt_dedupe::register_provider_session("codex", logical, logical);
    crate::services::tui_prompt_dedupe::register_codex_herdr_placeholder(logical, channel);
    hold(nonce).inspect_err(|_| {
        if owned {
            crate::services::claude_tui::owner_input::vetoed("hold_unavailable");
        }
    })?;
    #[cfg(test)]
    crate::services::provider::herdr_before_start::test_input_boundary(cancel);
    let mut denied = false;
    let run = observed_input(
        cancel,
        nonce,
        || None,
        || {
            let _permit = match checked
                .as_ref()
                .map(|checked| checked.begin_effect())
                .transpose()
            {
                Ok(permit) => permit,
                Err(_) => {
                    denied = true;
                    return PlanRun {
                        run: InputRun::Refused(
                            crate::services::codex_tui::host_input::InputRefusal::Conflict,
                        ),
                        composer_mutated: false,
                        enter_attempted: false,
                    };
                }
            };
            run_herdr(&target, &plan, cancel)
        },
    );
    if let Some(scope) = epoch_scope {
        scope.finish_start();
    }
    let run = run?;
    if denied {
        release_closed_hold(logical, nonce);
        return Err(owned_hold("clear_epoch_changed"));
    }
    let run = match run {
        crate::services::provider::herdr_before_start::InputRun::Ran(run) => run,
        crate::services::provider::herdr_before_start::InputRun::Closed => {
            release_closed_hold(logical, nonce);
            if owned {
                return Err(owned_hold("input_closed"));
            }
            return Ok(None);
        }
    };
    if run.run != InputRun::Applied {
        if composer_untouched(&run) {
            warn_release(logical, release_hold(nonce));
            if owned {
                crate::services::claude_tui::owner_input::first_key_held("herdr_input_untouched");
                return Err(owned_hold("gate_refused"));
            }
        }
        legacy_result(run.run)?;
        return Ok(None);
    }
    if draft {
        crate::services::codex_tui::input::notify_owned_draft(channel);
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
    let context =
        crate::services::tui_prompt_dedupe::binding_context::execution_context("codex", nonce)?;
    let offset = crate::services::tui_prompt_dedupe::binding_context::herdr_resume::start_offset(
        &context, &source,
    )?;
    let record = current_record(turn, runtime, nonce)?;
    if !ports.attach(&turn.owner, &record, &source, &target)? {
        return Err(format!("herdr turn: codex execution {nonce} is not bound"));
    }
    // A resumed source keeps the boundary recorded before its successor started.
    let start = HerdrTurnStart {
        execution_nonce: nonce.to_owned(),
        source: source.path.clone(),
        file: Some((source.dev, source.ino)),
        offset,
        submitted_at: None,
    };
    record_turn_start(turn.cancel.as_deref(), start)?;
    warn_release(logical, release_hold(nonce));
    read_reply(
        turn,
        &target,
        &source.path,
        offset,
        source.session_id.clone(),
        sender,
    )
}

#[cfg(test)]
mod pg_tests;
