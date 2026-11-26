//! The gated content write returns before Source and provider completion observation.
use super::*;
use crate::services::claude_tui::guarded_outcome::{self, SubmissionError};

pub(crate) struct Submitted {
    attached: Attached,
    transcript: String,
    before: u64,
    started_at: chrono::DateTime<chrono::Utc>,
}

#[cfg(any(test, feature = "test-support"))]
impl Submitted {
    /// The transcript length before the write, for tests outside this module.
    pub(crate) fn before_for_test(&self) -> u64 {
        self.before
    }
}

#[cfg(any(test, feature = "test-support"))]
impl Attached {
    /// An attached source built field by field, for tests outside `herdr_turn`.
    pub(crate) fn for_test(
        nonce: String,
        session_id: String,
        transcript: PathBuf,
        target: HerdrTarget,
        bound: bool,
        awaiting_clear: bool,
        physical: Option<(String, String)>,
    ) -> Self {
        Self {
            nonce,
            session_id,
            transcript,
            target,
            bound,
            awaiting_clear,
            physical,
        }
    }

    pub(crate) fn nonce_for_test(&self) -> &str {
        &self.nonce
    }

    pub(crate) fn session_id_for_test(&self) -> &str {
        &self.session_id
    }

    pub(crate) fn target_for_test(&self) -> &HerdrTarget {
        &self.target
    }
}

/// Posts the hold's one notice row; keyed by execution and hold, so a repeat returns the row.
pub(crate) type Notice = dyn Fn(&str) -> Result<i64, String> + Send + Sync;

pub(super) fn submit(
    turn: &HerdrTurn<'_>,
    attached: Attached,
    sender: &Sender<StreamMessage>,
) -> Result<Option<Submitted>, String> {
    let (logical, cancel) = (turn.owner.logical_key.as_str(), turn.cancel.as_deref());
    write(logical, cancel, attached, turn.prompt, sender, None)
        .map_err(SubmissionError::into_legacy)
}

/// What a Pending row's turn hands the owner: the source its attach returned, and the lineage
/// read from that source, which may name a fresh launch after an unplaced Pending was retired.
pub(super) fn pending_handover(
    record: &HostedExecution,
    source: impl FnOnce(&HostedExecution) -> Result<Attached, String>,
) -> Result<(Attached, crate::services::claude_tui::owner_input::Lineage), String> {
    let attached = source(record)?;
    let nonce = attached.nonce.clone();
    let lineage = crate::services::claude_tui::owner_input::Lineage::PendingAttached { nonce };
    Ok((attached, lineage))
}

/// A test's Pending turn handed to the owner hook on this thread: the snapshot `record` the turn
/// read, the execution `attached` its attach returned, and the execution `target` the hook names.
#[cfg(all(test, unix))]
pub(crate) fn owned_pending_prompt_for_tests(
    turn: &HerdrTurn<'_>,
    (record, attached, target): (&HostedExecution, &HostedExecution, &HostedExecution),
    (session_id, transcript): (&str, PathBuf),
) -> Result<(), String> {
    let hook = crate::services::claude_tui::owner_input::launch_hook().ok_or("no owner hook")?;
    let source = |record: &HostedExecution| -> Result<Attached, String> {
        Ok(Attached {
            nonce: record.execution_nonce.clone(),
            session_id: session_id.into(),
            transcript: transcript.clone(),
            target: gate(record)?,
            bound: true,
            awaiting_clear: false,
            physical: physical(record),
        })
    };
    let (_, lineage) = pending_handover(record, |_| source(attached))?;
    let (sender, _receiver) = std::sync::mpsc::channel();
    submit_owned(turn, hook, (lineage, None), source(target)?, &sender).map(drop)
}

/// The prompt through the owner waiting on this thread: it reserves the frame on the execution
/// the driver observed, binds its pane, and runs this write as its effect; its settlement is final.
pub(super) fn submit_owned(
    turn: &HerdrTurn<'_>,
    hook: crate::services::claude_tui::owner_input::LaunchHook,
    (lineage, post): (
        crate::services::claude_tui::owner_input::Lineage,
        Option<Arc<HoldNotice>>,
    ),
    attached: Attached,
    sender: &Sender<StreamMessage>,
) -> Result<Submitted, String> {
    let (socket, pane) = attached
        .physical
        .clone()
        .ok_or("herdr turn: the execution has no pane")?;
    let logical = turn.owner.logical_key.clone();
    let target = serde_json::json!({"host":"herdr","owner":logical,"root":socket,
        "execution_nonce":attached.nonce});
    let transcript = attached.transcript.display().to_string();
    let source = match std::fs::metadata(&attached.transcript) {
        Ok(meta) if lineage == crate::services::claude_tui::owner_input::Lineage::Existing => {
            use std::os::unix::fs::MetadataExt;
            serde_json::json!({"kind":"existing","session_id":attached.session_id,
                "transcript":transcript,"file":[meta.dev(), meta.ino()],"offset":meta.len()})
        }
        _ => serde_json::json!({"kind":"pre_source","expected_session_id":attached.session_id,
            "expected_transcript":transcript,"binding_watermark":null}),
    };
    let slot = Arc::new(std::sync::Mutex::new(None));
    let (ticket, cancel, sender) = (slot.clone(), turn.cancel.clone(), sender.clone());
    let epoch_scope = clear_epoch::current();
    let nonce = attached.nonce.clone();
    let notice = post.map(|post| Arc::new(move |hold: &str| post(&nonce, hold)) as Arc<Notice>);
    let submit = Box::new(move |frame: String| {
        let _epoch = clear_epoch::install(epoch_scope);
        guarded_outcome::capture_preparation(|| {
            let notice = notice.as_deref();
            let written = write(
                &logical,
                cancel.as_deref(),
                attached,
                &frame,
                &sender,
                notice,
            )?;
            let submitted = written.ok_or_else(|| {
                before_payload(&logical, "input cancelled before first key", None)
            })?;
            *ticket.lock().unwrap_or_else(|e| e.into_inner()) = Some(submitted);
            Ok(())
        })
    });
    hook(
        lineage,
        target,
        source,
        Some(format!("{socket}#{pane}")),
        submit,
    )?;
    let submitted = slot.lock().unwrap_or_else(|e| e.into_inner()).take();
    submitted.ok_or_else(|| "herdr turn: the owner submitted without a ticket".into())
}

/// The direct submit, for tests outside this module.
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn submit_for_test(
    turn: &HerdrTurn<'_>,
    attached: Attached,
    sender: &Sender<StreamMessage>,
) -> Result<Option<Submitted>, String> {
    submit(turn, attached, sender)
}

/// The owned submit, for tests outside this module.
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn submit_owned_for_test(
    turn: &HerdrTurn<'_>,
    hook: crate::services::claude_tui::owner_input::LaunchHook,
    lineage: (
        crate::services::claude_tui::owner_input::Lineage,
        Option<Arc<HoldNotice>>,
    ),
    attached: Attached,
    sender: &Sender<StreamMessage>,
) -> Result<Submitted, String> {
    submit_owned(turn, hook, lineage, attached, sender)
}

/// The gated content write, for tests outside this module.
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn write_for_test(
    logical: &str,
    cancel: Option<&CancelToken>,
    attached: Attached,
    prompt: &str,
    sender: &Sender<StreamMessage>,
    notice: Option<&Notice>,
) -> Result<Option<Submitted>, SubmissionError> {
    write(logical, cancel, attached, prompt, sender, notice)
}

/// The v2 hold record a write leaves before its first key, for tests outside this module.
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn held_record_for_test(
    logical: &str,
    attached: &Attached,
    prompt: &str,
    before: u64,
    started_at: chrono::DateTime<chrono::Utc>,
) -> hold_record::HoldRecord {
    held_record(logical, attached, prompt, before, started_at)
}

/// The hold's final outcome write, for tests outside this module.
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn record_outcome_for_test(record: hold_record::HoldRecord, run: &InputRun) {
    record_outcome(record, run)
}

fn write(
    logical: &str,
    cancel: Option<&CancelToken>,
    attached: Attached,
    prompt: &str,
    sender: &Sender<StreamMessage>,
    notice: Option<&Notice>,
) -> Result<Option<Submitted>, SubmissionError> {
    let epoch_scope = clear_epoch::current();
    let checked = epoch_scope
        .as_ref()
        .map(|scope| scope.check_epoch())
        .transpose()
        .map_err(|error| before_payload(logical, &error, None))?;
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
        TuiInputAction::PasteBuffer(prompt.to_owned()),
        TuiInputAction::Enter,
    ];
    let record = held_record(logical, &attached, prompt, before, started_at);
    let ports = hold_resolve::LivePorts {
        target: &attached.target,
        notice,
    };
    let (kept, failed) = (std::cell::Cell::new(false), std::cell::RefCell::new(None));
    let seen = std::cell::RefCell::new(None);
    #[cfg(test)]
    before_start::test_input_boundary(cancel);
    let start = || {
        let mut start = HerdrTurnStart::at_end_of(&attached.nonce, path, Some(started_at));
        start.offset = before;
        start
    };
    let mut denied = None;
    let result = observed_input(cancel, start, || {
        crate::services::claude_tui::composer_lock::with_composer_mutation_lock(logical, || {
            let _permit = match checked
                .as_ref()
                .map(|checked| checked.begin_effect())
                .transpose()
            {
                Ok(permit) => permit,
                Err(detail) => {
                    denied = Some(detail);
                    return InputRun::Refused(
                        crate::services::claude_tui::host_input::InputRefusal::Conflict,
                    );
                }
            };
            // The earlier hold is judged, this hold recorded, keys sent and the outcome recorded
            // under one lock, so no pass ever reads a hold whose write still runs.
            let pending = match crate::services::claude_tui::owner_input::active() {
                true => !matches!(
                    hold_resolve::judge(logical, &attached.nonce, &ports, &mut seen.borrow_mut()),
                    Ok(hold_resolve::Verdict::Clear(_))
                ),
                false => input_held(&attached.nonce).unwrap_or(true),
            };
            if pending {
                kept.set(true);
                return InputRun::Refused(
                    crate::services::claude_tui::host_input::InputRefusal::Unknown,
                );
            }
            if let Err(error) = hold_record::write(&record) {
                *failed.borrow_mut() = Some(error);
                return InputRun::Refused(
                    crate::services::claude_tui::host_input::InputRefusal::Unknown,
                );
            }
            let run = (|| {
                if attached.awaiting_clear {
                    match composer::wait_for(cancel, Duration::from_secs(10), || {
                        if crate::services::claude_tui::readiness::adopt_awaiting_clear(
                            logical,
                            &attached.nonce,
                            path,
                        ) {
                            std::ops::ControlFlow::Break(())
                        } else {
                            std::ops::ControlFlow::Continue(())
                        }
                    }) {
                        Ok(()) => (),
                        Err(composer::WaitStop::Cancelled) => {
                            return InputRun::Cancelled { confirmed: 0 };
                        }
                        Err(composer::WaitStop::TimedOut) => {
                            return InputRun::Refused(
                                crate::services::claude_tui::host_input::InputRefusal::Unknown,
                            );
                        }
                    }
                }
                let _ = composer::take_touched();
                composer::run_ready(logical, path, &attached.target, &plan, cancel)
            })();
            record_outcome(record.clone(), &run);
            run
        })
    });
    if let Some(scope) = epoch_scope {
        scope.finish_start();
    }
    if let Some(detail) = denied {
        return Err(before_payload(logical, &detail, None));
    }
    let HerdrInputRun::Ran(run) = result? else {
        return Ok(None);
    };
    if kept.get() {
        let target = attached.target.clone();
        let (nonce, seen) = (attached.nonce.clone(), seen.into_inner());
        let probe = hold_resolve::watch(nonce, seen, move || target.capture_ansi(-80));
        crate::services::claude_tui::owner_input::record_preparation_watch(probe);
        return Err(before_payload(logical, "herdr_input_hold_pending", None));
    }
    if let Some(error) = failed.into_inner() {
        return Err(error.into());
    }
    crate::services::tui_o::exact_submission::observe_untouched(matches!(
        run,
        InputRun::Refused(_) | InputRun::Cancelled { confirmed: 0 }
    ));
    match run {
        InputRun::Applied => {
            crate::logging::intake_timing::mark(crate::logging::intake_timing::Stage::Applied)
        }
        stopped @ (InputRun::Refused(_) | InputRun::Cancelled { confirmed: 0 }) => {
            let pane = attached.target.clone();
            let watched_session = logical.to_owned();
            let clearing = attached.awaiting_clear;
            let epoch = attached.nonce.clone();
            let clear_path = path.to_path_buf();
            let watched_path = attached.transcript.clone();
            crate::services::claude_tui::owner_input::record_preparation_watch(Box::new(
                move || {
                    use crate::services::claude_tui::busy_inject::Screen;
                    // The same restart-aware proof the composer admits, with its screen check.
                    let restart = (!clearing).then(|| {
                        crate::services::claude_tui::readiness::observe_restored(
                            &watched_session,
                            Some(&watched_path),
                        )
                        .map(|proof| proof.restart())
                    });
                    let ready = match restart {
                        None => crate::services::claude_tui::readiness::clear_wakeable(
                            &watched_session,
                            &epoch,
                            &clear_path,
                        ),
                        Some(proof) => proof.is_some(),
                    };
                    if ready
                        && pane.capture_ansi(-80).as_deref().is_some_and(|capture|
                            crate::services::claude_tui::readiness::empty_draft(capture)
                            && !crate::services::tmux_common::tmux_capture_indicates_claude_tui_interactive_modal(capture)
                            && !composer::dialog_open(capture)
                            && (restart != Some(Some(true)) || composer::restored_screen_idle(capture))) {
                        Screen::Ready
                    } else { Screen::Unknown }
                },
            ));
            use crate::services::claude_tui::host_input::InputRefusal;
            let unproven = match stopped {
                InputRun::Refused(InputRefusal::Unproven(end)) => Some(end),
                _ => None,
            };
            let modal = matches!(
                stopped,
                InputRun::Refused(InputRefusal::Waits(
                    crate::services::claude_tui::readiness::WaitCause::Modal
                ))
            );
            let detail = format!("Claude composer held before first key: {stopped:?}");
            let mut held = before_payload(logical, &detail, unproven);
            if let SubmissionError::Held(hold) = &mut held {
                hold.modal = modal;
            }
            return Err(held);
        }
        stopped => return Err(SubmissionError::Guarded(stopped)),
    }
    Ok(Some(Submitted {
        attached,
        transcript,
        before,
        started_at,
    }))
}

/// The v2 hold this write records before its first key: `in_flight`, no proof, no text.
fn held_record(
    logical: &str,
    attached: &Attached,
    prompt: &str,
    before: u64,
    started_at: chrono::DateTime<chrono::Utc>,
) -> hold_record::HoldRecord {
    use std::os::unix::fs::MetadataExt;
    let file = std::fs::metadata(&attached.transcript).ok();
    hold_record::HoldRecord {
        schema: 2,
        recorded_at: chrono::Utc::now().to_rfc3339(),
        provider: "claude".into(),
        logical_key: logical.into(),
        execution_nonce: attached.nonce.clone(),
        hold_id: uuid::Uuid::new_v4().simple().to_string(),
        frame_sha256: hold_record::sha256_hex(prompt.as_bytes()),
        screen_exact: hold_record::screen_exact(prompt),
        transcript: attached.transcript.display().to_string(),
        file: file.map(|meta| (meta.dev(), meta.ino())),
        offset_before: before,
        started_at: started_at.to_rfc3339(),
        send_outcome: hold_record::SendOutcome::InFlight,
        outcome_at: None,
        unsent_proof: hold_record::UnsentProof::None,
        cleanup_phase: hold_record::CleanupPhase::None,
        cleanup_at: None,
        reservation: None,
        unclear_passes: 0,
        notice_intent_at: None,
        notice_enqueued_at: None,
    }
}

/// Ends the hold of an untouched or submitted composer; otherwise rewrites `in_flight` once with
/// the final outcome and entrusts the touched reservation to the hold.
fn record_outcome(mut record: hold_record::HoldRecord, run: &InputRun) {
    let touched = composer::take_touched();
    let Some((outcome, proof)) = hold_record::outcome(run) else {
        if let super::HoldRelease::Kept(error) = super::release_hold(&record.execution_nonce) {
            tracing::warn!(logical = %record.logical_key, %error, "herdr turn: input hold kept");
        }
        return;
    };
    (record.send_outcome, record.unsent_proof) = (outcome, proof);
    record.outcome_at = Some(chrono::Utc::now().to_rfc3339());
    record.reservation = touched.as_ref().map(|r| r.revision());
    if let Some(reservation) = touched {
        hold_resolve::entrust(&record.hold_id, reservation);
    }
    if let Err(error) = hold_record::write(&record) {
        tracing::warn!(logical = %record.logical_key, %error, "herdr turn: outcome not recorded");
    }
}

fn before_payload(
    logical: &str,
    detail: &str,
    unproven: Option<crate::services::claude_tui::readiness::Unproven>,
) -> SubmissionError {
    crate::services::claude_tui::owner_input::vetoed(detail);
    SubmissionError::Held(crate::services::claude_tui::input::PreparationHold {
        target: logical.into(),
        reason: "herdr_composer_not_ready",
        phase: crate::services::claude_tui::input::EffectPhase::BeforePayload,
        detail: detail.into(),
        modal: false,
        unproven,
    })
}

/// Runs `publish` under the pane's source authority unless a termination chose execution `nonce`:
/// the input it already sent stays settled, but nothing more of it is published or bound.
fn publishes(
    logical: &str,
    nonce: &str,
    publish: impl FnOnce(&crate::services::tmux_common::TmuxSourceAuthority<'_>),
) -> bool {
    crate::services::tmux_common::with_tmux_source_authority(logical, |authority| {
        let live = !crate::services::session_host::execution_closing(nonce);
        if live {
            publish(authority);
        }
        live
    })
}

pub(super) fn observe(
    turn: &HerdrTurn<'_>,
    runtime: &Handle,
    submitted: Submitted,
    sender: Sender<StreamMessage>,
) -> Result<(), String> {
    use crate::services::claude_tui::{readiness, source_verify};
    use crate::services::tui_prompt_dedupe::{self as dedupe, binding_context};

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
    let launch_start = if attached.bound {
        None
    } else {
        readiness::launch_start_hook(logical, &attached.nonce, &attached.session_id, path)
    };
    let transcript_ready = || {
        use source_verify::{FirstRecord, OpenedTranscript};
        if launch_start.is_none() {
            return path.exists();
        }
        // An empty or partial first record cannot verify the launch source yet.
        !matches!(
            source_verify::observe_transcript(path),
            Ok(OpenedTranscript::Missing
                | OpenedTranscript::Opened {
                    first: FirstRecord::NotWritten,
                    ..
                })
        )
    };
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
        transcript_ready,
        || true,
        unread,
    )?;
    if early.is_some() {
        return Ok(());
    }
    let binding = |last_offset| TuiRuntimeBinding {
        runtime_kind: RuntimeHandoffKind::ClaudeTui,
        output_path: transcript.clone(),
        relay_output_path: None,
        input_fifo_path: None,
        session_id: Some(attached.session_id.clone()),
        last_offset,
        relay_last_offset: None,
    };
    // Resolve and verify the launch source before the first read exposes the busy turn.
    let mut bound = attached.bound;
    if !bound {
        let live = publishes(logical, &attached.nonce, |authority| {
            let published = dedupe::register_launched_tmux_runtime_binding_under_source_authority(
                authority,
                binding(start),
            );
            if let (true, Some(hook)) = (published, launch_start.as_ref())
                && dedupe::herdr_execution_is(logical, &attached.nonce)
                && matches!(binding_context::observe_spawn_nonce_marker(logical),
                    binding_context::SpawnNonceMarker::Known(nonce) if nonce == attached.nonce)
                && let Err(error) =
                    dedupe::reclaim_with_current_prompt(authority, &attached.session_id, hook)
            {
                tracing::warn!(logical, %error, "Claude launch source verification not persisted");
            }
        });
        bound = live && runtime.block_on(bind_once_logged(turn, &attached.nonce));
    }
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
    if unconfirmed && woken {
        return Ok(());
    }
    // A Herdr turn hands off exactly where its terminal record ends, never past it.
    let terminal_end = terminal.as_ref().map(|(read, ..)| read.end);
    let handoff_end = || terminal_end.unwrap_or_else(length);
    // Registered again at the handoff offset, before any terminal of this turn reaches admission.
    let live = publishes(logical, &attached.nonce, |authority| {
        let handoff = binding(handoff_end());
        dedupe::register_launched_tmux_runtime_binding_under_source_authority(authority, handoff);
    });
    if live && !bound {
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

#[cfg(all(any(test, feature = "test-support"), unix))]
pub(super) fn observe_cold_for_tests(
    turn: &HerdrTurn<'_>,
    attached: Attached,
    sender: Sender<StreamMessage>,
) -> Result<(), String> {
    let submitted = Submitted {
        transcript: attached.transcript.display().to_string(),
        attached,
        before: 0,
        started_at: chrono::Utc::now(),
    };
    observe(turn, &Handle::current(), submitted, sender)
}

#[cfg(test)]
#[test]
fn c132_early_cancel_never_binds_or_announces_runtime() {
    regression_tests::c132_early_cancel_never_binds_or_announces_runtime();
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) mod regression_tests {
    use super::*;
    pub(super) fn c132_early_cancel_never_binds_or_announces_runtime() {
        use crate::services::session_host::herdr_socket_rig_tests::{HerdrRig, KEY, NODE, SESSION};
        let root = tempfile::tempdir().unwrap();
        let root_guard = crate::config::set_agentdesk_root_for_test(root.path());
        let rig = HerdrRig::start();
        let registry_guard = rig.registry_on_this_thread();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let entered = runtime.enter();
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://localhost/unused_c132")
            .unwrap();
        for cleanup in [false, true] {
            let logical = format!("c132-{}", uuid::Uuid::new_v4());
            let transcript = root.path().join(format!("{logical}.jsonl"));
            let (owner, attached) = regression_attached(&rig, &logical, &transcript);
            let token = Arc::new(CancelToken::new());
            token
                .cancelled
                .store(true, std::sync::atomic::Ordering::Release);
            if cleanup {
                token.mark_completion_cleanup();
            }
            let turn = HerdrTurn {
                pool: &pool,
                owner,
                channel_id: 1,
                endpoint: HerdrLaunchEndpoint {
                    execution_node: NODE.into(),
                    config_key: KEY.into(),
                    socket_addr: rig.socket().display().to_string(),
                    herdr_session: SESSION.into(),
                },
                row: None,
                prompt: "never sent",
                working_dir: "/tmp",
                session_id: None,
                force_fresh: false,
                system_prompt: None,
                model: None,
                hook_endpoint: None,
                cancel: Some(token),
            };
            let submitted = Submitted {
                attached,
                transcript: transcript.display().to_string(),
                before: 17,
                started_at: chrono::Utc::now(),
            };
            let (sender, receiver) = std::sync::mpsc::channel();
            observe(&turn, &runtime.handle(), submitted, sender).unwrap();
            let frames: Vec<_> = receiver.try_iter().collect();
            assert!(
                !frames
                    .iter()
                    .any(|frame| matches!(frame, StreamMessage::RuntimeReady { .. })),
                "early cancellation announced a successful runtime: cleanup={cleanup}"
            );
            assert!(
                crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(&logical)
                    .is_none(),
                "early cancellation registered a runtime: cleanup={cleanup}"
            );
            assert!(!transcript.exists());
        }
        drop(entered);
        drop(registry_guard);
        drop(root_guard);
    }

    pub(crate) fn regression_attached(
        rig: &crate::services::session_host::herdr_socket_rig_tests::HerdrRig,
        logical: &str,
        transcript: &Path,
    ) -> (HostedOwner, Attached) {
        use crate::db::dispatched_sessions::hosted_execution::{
            HostedExecution, HostedLocation, SourceRef,
        };
        use crate::services::session_host::herdr_socket_rig_tests::{KEY, NODE, PANE, SESSION};
        let nonce = "00112233445566778899aabbccddee01";
        let owner = HostedOwner {
            provider: "claude".into(),
            discord_token_hash: "test".into(),
            channel_id: "1".into(),
            logical_key: logical.into(),
            owner_node: NODE.into(),
            runtime_root: "test".into(),
        };
        let record = HostedExecution {
            schema: 1,
            state: HostedState::Bound,
            execution_nonce: nonce.into(),
            owner: owner.clone(),
            location: Some(HostedLocation {
                host: "herdr".into(),
                execution_node: NODE.into(),
                endpoint_config_key: KEY.into(),
                socket_addr: rig.socket().display().to_string(),
                named_session: SESSION.into(),
                pane_id: PANE.into(),
            }),
            expected: Some(rig.expected(nonce)),
            source_ref: SourceRef {
                runtime_root: "test".into(),
                channel: "1".into(),
                provider: "claude".into(),
                logical_key: logical.into(),
                execution_nonce: nonce.into(),
                initial_source: None,
                baseline_event_seq: None,
            },
        };
        crate::services::claude_tui::readiness::test_support::pre_source(logical, transcript);
        (
            owner,
            Attached {
                nonce: nonce.into(),
                session_id: "regression-native".into(),
                transcript: transcript.to_path_buf(),
                target: herdr_endpoints().target(&record).unwrap(),
                bound: true,
                awaiting_clear: false,
                physical: None,
            },
        )
    }
}
