//! The gated content write returns before Source and provider completion observation.
use super::*;
use crate::services::claude_tui::guarded_outcome::{self, SubmissionError};

pub(super) struct Submitted {
    attached: Attached,
    transcript: String,
    before: u64,
    started_at: chrono::DateTime<chrono::Utc>,
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
                            && (restart != Some(Some(true)) || composer::restored_screen_idle(capture))) {
                        Screen::Ready
                    } else { Screen::Unknown }
                },
            ));
            let unproven = match stopped {
                InputRun::Refused(
                    crate::services::claude_tui::host_input::InputRefusal::Unproven(end),
                ) => Some(end),
                _ => None,
            };
            return Err(before_payload(
                logical,
                &format!("Claude composer held before first key: {stopped:?}"),
                unproven,
            ));
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
        crate::services::tmux_common::with_tmux_source_authority(logical, |authority| {
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
        bound = runtime.block_on(bind_once_logged(turn, &attached.nonce));
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
    dedupe::register_launched_tmux_runtime_binding(logical, binding(handoff_end()));
    if !bound {
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

#[cfg(all(test, unix))]
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
mod clear_held_tests;
#[cfg(test)]
mod hold_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod unproven_tests;

#[cfg(test)]
#[test]
fn c132_early_cancel_never_binds_or_announces_runtime() {
    regression_tests::c132_early_cancel_never_binds_or_announces_runtime();
}

#[cfg(test)]
mod regression_tests {
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

    pub(super) fn regression_attached(
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
        crate::services::claude_tui::readiness::tests::pre_source(logical, transcript);
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

    #[test]
    fn herdr_composer_refusal_keeps_real_owner_proof_and_wake_then_submits_once() {
        use crate::services::discord::{
            owner_drive_for_test as drive, owner_effect_for_test as effect,
            owner_held_watch_for_test as held_watch,
        };
        use crate::services::session_host::herdr_socket_rig_tests::{
            HerdrRig, KEY, NODE, PANE, SESSION,
        };
        let root = tempfile::tempdir().unwrap();
        let root_guard = crate::config::set_agentdesk_root_for_test(root.path());
        let rig = HerdrRig::start();
        let registry_guard = rig.registry_on_this_thread();
        let context = rig.context("00112233445566778899aabbccddee01");
        rig.run_provider(&context, false);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .on_thread_start(rig.thread_installer(NODE, KEY, rig.socket(), SESSION))
            .enable_all()
            .build()
            .unwrap();
        let entered = runtime.enter();
        let channel = 727487;
        let logical = "herdr-held-regression";
        let marker = crate::services::tmux_common::session_temp_path(logical, "host_kind");
        std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
        std::fs::write(marker, "herdr").unwrap();
        let transcript = root.path().join("not-created.jsonl");
        let registry = crate::services::turn_orchestrator::ChannelMailboxRegistry::default();
        let mailbox = registry.handle(poise::serenity_prelude::ChannelId::new(channel));
        let target = serde_json::json!({"host":"herdr","owner":logical,"root":rig.socket().display().to_string(),"execution_nonce":"00112233445566778899aabbccddee01"});
        let attempt = serde_json::json!({"nonce":"held-attempt-1","provider":"claude","target":target,"source":{"kind":"pre_source","expected_session_id":null,"expected_transcript":null,"binding_watermark":null}});
        runtime
            .block_on(mailbox.v2_cold_fixture(
                root.path().join("held.json"),
                attempt.clone(),
                "one original".into(),
                channel,
            ))
            .unwrap();
        let capture = |draft: &str| serde_json::json!({"type":"pane_read","read":{"pane_id":PANE,"source":"recent_unwrapped","text":format!("✻ Baked for 2s\n\n{}\n❯ {draft}\n{}\n  🤖 Opus(H) │ 7% │ MCP: 2\n  ⏵⏵ bypass permissions on (shift+tab to cycle)","─".repeat(52),"─".repeat(52)),"revision":1,"truncated":false}});
        rig.answer("pane.read", capture("human draft"));
        let (_, attached) = regression_attached(&rig, logical, &transcript);
        let writer = format!("{}#{}", rig.socket().display(), PANE);
        let leaf = effect::created(
            Box::new(move |frame: String| {
                guarded_outcome::capture_preparation(|| {
                    let (sender, receiver) = std::sync::mpsc::channel();
                    let result = write(logical, None, attached, &frame, &sender, None).map(drop);
                    drop(receiver);
                    result
                })
            }),
            Some(writer.clone()),
        );
        let result = runtime
            .block_on(effect::run(&mailbox, Arc::new(|| 10000), leaf))
            .unwrap();
        assert!(matches!(result, effect::Completion::NotSent(_)));
        let snapshot = runtime
            .block_on(mailbox.v2_request(|slot| slot.as_ref().unwrap().json()))
            .unwrap();
        assert!(snapshot["head_injection"].is_null());
        assert_eq!(snapshot["items"].as_array().unwrap().len(), 1);
        assert!(snapshot["retained_inputs"].as_array().unwrap().is_empty());
        assert!(rig.sends().is_empty());
        let message = snapshot["items"][0]["message_id"].as_u64().unwrap();
        rig.answer("pane.read", capture(""));
        let (woken, wake) = tokio::sync::oneshot::channel();
        let woken = Arc::new(std::sync::Mutex::new(Some(woken)));
        let drive = Arc::new(drive::Drive {
            mailbox: mailbox.clone(),
            channel,
            handoff: Arc::new(move |_| {
                if let Some(woken) = woken.lock().unwrap().take() {
                    woken.send(()).unwrap();
                }
                Box::pin(async { Err("observed actual owner wake".into()) })
            }),
            settled: None,
        });
        let observed = runtime.block_on(async {
            let (_, observed) = held_watch::watch_observed(drive, message);
            observed.await.unwrap()
        });
        assert_eq!(observed.verdict, held_watch::Verdict::Wake);
        runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(5), wake)
                .await
                .unwrap()
                .unwrap()
        });
        let second = attempt.clone();
        runtime.block_on(mailbox.v2_request(move |slot| {
        let owner = slot.as_mut().unwrap();
        let mut second = second; second["nonce"] = "held-attempt-2".into();
        owner.reserve(serde_json::from_value(second).unwrap(), serde_json::from_value(serde_json::json!({"frame":"one original","digest":"fixture","attribution":{}})).unwrap()).map(drop)
    })).unwrap();
        let (_, attached) = regression_attached(&rig, logical, &transcript);
        let leaf = effect::created(
            Box::new(move |frame: String| {
                guarded_outcome::capture_preparation(|| {
                    let (sender, receiver) = std::sync::mpsc::channel();
                    let result = write(logical, None, attached, &frame, &sender, None).map(drop);
                    drop(receiver);
                    result
                })
            }),
            Some(writer),
        );
        assert_eq!(
            runtime
                .block_on(effect::run(&mailbox, Arc::new(|| 10001), leaf))
                .unwrap(),
            effect::Completion::Submitted
        );
        assert_eq!(
            rig.sends().len(),
            2,
            "one paste and one Enter after the wake"
        );
        let snapshot = runtime
            .block_on(mailbox.v2_request(|slot| slot.as_ref().unwrap().json()))
            .unwrap();
        assert!(snapshot["items"].as_array().unwrap().is_empty());
        assert!(snapshot["retained_inputs"].as_array().unwrap().is_empty());
        drop(entered);
        drop(registry_guard);
        drop(root_guard);
    }
}
