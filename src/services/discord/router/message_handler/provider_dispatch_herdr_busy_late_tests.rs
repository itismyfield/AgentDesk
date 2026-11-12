//! A real owned late hold recovers only its unchanged front through a verified clear.
use super::*;
use crate::services::discord::queue_io::input_owner::drive::{Drive, Handoff, wake};
use crate::services::discord::router::message_handler::provider_dispatch as dispatch;
use crate::services::tui_prompt_dedupe::binding_events::{
    BindingTarget, binding_events_since, subscribe_binding_events,
};

type Passes = tokio::sync::mpsc::UnboundedReceiver<Option<Result<(), String>>>;

fn actual_drive(intake: &Intake) -> (Arc<Drive>, Passes) {
    let shared = intake.shared.clone();
    let channel = ChannelId::new(CHANNEL);
    let idle: Handoff = Arc::new(move |front| {
        let shared = shared.clone();
        Box::pin(dispatch::start_idle_for_test(shared, channel, front))
    });
    let mut drive = dispatch::owner_drive_for_test(
        (intake.mailbox.clone(), intake.shared.pg_pool.clone()),
        (ProviderKind::Claude, CHANNEL),
        idle,
        Some(intake.shared.clone()),
    );
    let actual = drive.handoff.clone();
    let result = Arc::new(Mutex::new(None));
    let handed = result.clone();
    Arc::get_mut(&mut drive).unwrap().handoff = Arc::new(move |front| {
        let (actual, handed) = (actual.clone(), handed.clone());
        Box::pin(async move {
            let result = actual(front).await;
            *handed.lock().unwrap() = Some(result.clone());
            result
        })
    });
    let settled = drive.settled.clone();
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    Arc::get_mut(&mut drive).unwrap().settled = Some(Arc::new(move || {
        let (result, settled, sender) = (result.clone(), settled.clone(), sender.clone());
        Box::pin(async move {
            if let Some(settled) = settled {
                settled().await;
            }
            let _ = sender.send(result.lock().unwrap().take());
        })
    }));
    (drive, receiver)
}

async fn queued(intake: &Intake) -> PendingQueueItem {
    let original = serde_json::from_value(json!({
        "author_id":7, "message_id":MESSAGE, "channel_id":CHANNEL,
        "source_message_ids":[MESSAGE], "queued_generation":1, "text":INPUT
    }))
    .unwrap();
    assert!(matches!(
        admit(&intake.mailbox, &original, AdmissionTicket::new()).await,
        OwnerAdmission::Accepted(_)
    ));
    intake
        .mailbox
        .v2_request(|slot| {
            let front = slot.as_ref().ok_or("no owner")?.front().ok_or("no front")?;
            Ok(front.1)
        })
        .await
        .unwrap()
}

async fn state(intake: &Intake) -> Value {
    intake
        .mailbox
        .v2_request(|slot| slot.as_ref().ok_or("no owner")?.json())
        .await
        .unwrap()
}

async fn next_pass(passes: &mut Passes) -> Option<Result<(), String>> {
    tokio::time::timeout(LIMIT, passes.recv())
        .await
        .expect("the actual owner drive settles")
        .expect("the drive stays observed")
}

fn unknown_source(started: &Started) {
    use std::os::unix::fs::MetadataExt;
    let before = std::fs::read(&started.path).unwrap();
    let identity = std::fs::metadata(&started.path).unwrap();
    append(
        &started.path,
        &[json!({"type":"permission-mode",
        "permissionMode":"bypassPermissions","sessionId":started.session})],
    );
    let current = std::fs::metadata(&started.path).unwrap();
    assert_eq!(
        (current.dev(), current.ino()),
        (identity.dev(), identity.ino())
    );
    assert!(
        std::fs::read(&started.path).unwrap().starts_with(&before),
        "the O reader's old bytes stay intact"
    );
    assert!(matches!(
        crate::services::tui_turn_state::observe_claude_jsonl_turn_state(&started.path),
        crate::services::tui_turn_state::TuiTurnState::Unknown
    ));
    assert!(
        host::channel_accepts(CHANNEL),
        "the real O actor is still owned and resumed"
    );
}

async fn late_unknown(
    fx: &Fixture,
    intake: &Intake,
    started: &Started,
) -> (Arc<Drive>, Passes, PendingQueueItem) {
    let before = dispatch::OwnerTurn::probes(CHANNEL);
    let original = queued(intake).await;
    let heads = write_on_enter(fx, intake, &started.session, &started.path, false);
    let (drive, mut passes) = actual_drive(intake);
    let (entered, release) = dispatch::OwnerTurn::pause_late(CHANNEL);
    wake(drive.clone());
    if let Err(error) = entered.recv_timeout(LIMIT) {
        let result = next_pass(&mut passes).await.expect("the real intake ran");
        assert_eq!(result, Ok(()), "the baseline actual Idle caller completes");
        let heads = heads.lock().unwrap();
        assert_eq!(
            heads.len(),
            2,
            "the baseline actual caller pasted and entered"
        );
        for head in heads.iter() {
            assert_eq!(head["original"]["message_id"], original.message_id);
            assert_eq!(
                head["original"]["queued_generation"],
                original.queued_generation
            );
        }
        assert!(
            dispatch::OwnerTurn::probes(CHANNEL).1 > before.1,
            "actual execute_owned sent the original without entering its late source observation: {error}"
        );
        panic!("the late observation did not publish its requested pause: {error}");
    }
    unknown_source(started);
    assert!(matches!(
        crate::services::tui_turn_state::observe_claude_jsonl_turn_state(&started.path),
        crate::services::tui_turn_state::TuiTurnState::Unknown
    ));
    release.send(()).unwrap();
    let result = next_pass(&mut passes)
        .await
        .expect("the first pass ran an actual intake");
    assert!(
        result
            .as_ref()
            .is_err_and(|error| error.starts_with("NotSent(")),
        "the unreadable late source must hold before any key: {result:?}"
    );
    let after = dispatch::OwnerTurn::probes(CHANNEL);
    assert_eq!((after.0 - before.0, after.1 - before.1), (1, 1));
    let held = state(intake).await;
    assert!(
        host::channel_accepts(CHANNEL),
        "the source hold is not an O ownership refusal"
    );
    assert_eq!(held["items"], json!([original]), "{held}");
    assert!(held["head_injection"].is_null(), "{held}");
    assert_eq!(held["retained_inputs"], json!([]), "{held}");
    (drive, passes, original)
}

async fn awaited(fx: &Fixture, nonce: &str, session: &str) -> PathBuf {
    let path = tokio::task::block_in_place(|| reconnect::clear_pending(fx, session));
    assert!(!path.exists(), "a clear Pending precedes its new JSONL");
    let clear = crate::services::claude::herdr_turn::awaited_clear(CHANNEL, fx.logical(), nonce)
        .expect("the same execution's canonical clear is verified");
    assert_eq!(clear.session_id, session);
    assert_eq!(clear.transcript.as_deref(), path.to_str());
    let raw: Option<Value> =
        sqlx::query_scalar("SELECT hosted_execution FROM sessions WHERE channel_id = $1")
            .bind(CHANNEL.to_string())
            .fetch_one(&fx.pool)
            .await
            .unwrap();
    let HostedRecord::Known(record) = HostedRecord::decode(raw.as_ref()) else {
        panic!("the same Bound execution remains recorded")
    };
    assert_eq!(record.execution_nonce, nonce);
    assert_eq!(record.state, HostedState::Bound);
    path
}

pub(super) fn write_on_enter(
    fx: &Fixture,
    intake: &Intake,
    session: &str,
    path: &Path,
    fresh: bool,
) -> Arc<Mutex<Vec<Value>>> {
    let (path, session) = (path.to_path_buf(), session.to_string());
    let body = Arc::new(Mutex::new(String::new()));
    let heads = Arc::new(Mutex::new(Vec::new()));
    let (seen, mailbox, runtime) = (
        heads.clone(),
        intake.mailbox.clone(),
        intake.runtime.handle().clone(),
    );
    fx.rig.on_send(move |params| {
        let snapshot = runtime
            .block_on(mailbox.v2_request(|slot| slot.as_ref().ok_or("no owner")?.json()))
            .unwrap();
        seen.lock().unwrap().push(snapshot["head_injection"].clone());
        if let Some(text) = params["text"].as_str() {
            let text = text
                .strip_prefix("\u{1b}[200~")
                .and_then(|text| text.strip_suffix("\u{1b}[201~"))
                .unwrap_or(text);
            *body.lock().unwrap() = text.to_string();
        }
        if params["keys"] == json!(["enter"]) {
            if fresh {
                assert!(!path.exists(), "no JSONL existed before the actual Enter");
            }
            let now = chrono::Utc::now().to_rfc3339();
            append(&path, &[
                json!({"type":"user","uuid":"p2-clear-user","sessionId":session,
                    "timestamp":now,"message":{"role":"user","content":body.lock().unwrap().clone()}}),
                json!({"type":"assistant","uuid":"p2-clear-answer","sessionId":session,
                    "apiBlockIndex":0,"timestamp":now,"message":{"id":"p2-clear-answer","role":"assistant",
                        "content":[{"type":"text","text":"the cleared session answered"}],"stop_reason":"end_turn"}}),
                json!({"type":"system","subtype":"turn_duration","sessionId":session,"timestamp":now}),
            ]);
        }
    });
    heads
}

fn resolved(session: &str, path: &Path, logical: &str, nonce: &str) -> bool {
    binding_events_since(CHANNEL, 0)
        .unwrap()
        .iter()
        .rev()
        .find(|event| {
            event.tmux_session == logical
                && event.execution_nonce.as_deref() == Some(nonce)
                && !matches!(event.new, BindingTarget::Rejected { .. })
        })
        .is_some_and(|event| {
            matches!(&event.new,
        BindingTarget::Resolved { source, .. }
            if source.session_id == session && source.path.as_path() == path)
        })
}

async fn resolved_and_settled(
    intake: &Intake,
    fx: &Fixture,
    session: &str,
    path: &Path,
    nonce: &str,
    heads: &Arc<Mutex<Vec<Value>>>,
    mut changes: tokio::sync::watch::Receiver<u64>,
) -> Value {
    tokio::time::timeout(LIMIT, async {
        while !resolved(session, path, fx.logical(), nonce) {
            changes
                .changed()
                .await
                .expect("the canonical binding log stays observed");
        }
        let attempt = heads
            .lock()
            .unwrap()
            .first()
            .expect("the actual retry pasted")["attempt"]["nonce"]
            .as_str()
            .unwrap()
            .to_string();
        loop {
            let snapshot = state(intake).await;
            let settled = snapshot["admission_binds"].as_array().is_some_and(|binds| {
                binds
                    .iter()
                    .any(|bind| bind["at"]["kind"] == "settled" && bind["at"]["attempt"] == attempt)
            });
            if settled {
                return snapshot;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the production watch drive resolves and settles the same attempt")
}

#[test]
fn held_unknown_reobserves_verified_clear_then_delegates_idle_pg() {
    let fx = Fixture::new("busy-late-clear-entry", None);
    let launcher = Arc::new(Launcher::default());
    let (nonce, started) = reconnect::launch(&fx, &launcher);
    let before = fx.rig.sends().len();
    let _registry = fx.rig.registry_on_this_thread();
    let _admission = open_admission();
    let intake = Intake::new(&fx, &started);
    intake.runtime.block_on(async {
        let (_drive, _passes, original) = late_unknown(&fx, &intake, &started).await;
        let held_late_reads = dispatch::OwnerTurn::probes(CHANNEL).1;
        assert_eq!(fx.rig.sends().len(), before, "the Held input keyed nothing");
        assert!(
            host::channel_accepts(CHANNEL),
            "the real O actor remains ready"
        );
        let session = uuid();
        let path = crate::services::claude_tui::transcript_tail::claude_transcript_path(
            fx.cwd.path(),
            &session,
            None,
        )
        .unwrap();
        let (retried, release) = dispatch::OwnerTurn::pause_late(CHANNEL);
        let heads = write_on_enter(&fx, &intake, &session, &path, true);
        assert_eq!(awaited(&fx, &nonce, &session).await, path);
        retried
            .recv_timeout(LIMIT)
            .expect("the source watch reaches actual execute_owned again");
        assert!(!path.exists(), "the retry reaches Idle with no new JSONL");
        let changes = subscribe_binding_events(CHANNEL).unwrap();
        release.send(()).unwrap();
        // The helper's production Drive owns this retry, so its canonical effects are observed.
        let final_state =
            resolved_and_settled(&intake, &fx, &session, &path, &nonce, &heads, changes).await;
        let sends = fx.rig.sends();
        assert_eq!(sends.len() - before, 2, "{sends:?}");
        assert!(sends[before]["text"].as_str().unwrap().contains(INPUT));
        assert_eq!(sends[before + 1]["keys"], json!(["enter"]));
        for head in heads.lock().unwrap().iter() {
            assert_eq!(head["original"]["message_id"], original.message_id);
            assert_eq!(
                head["original"]["queued_generation"],
                original.queued_generation
            );
            assert_eq!(head["attempt"]["target"]["execution_nonce"], nonce);
        }
        assert_eq!(heads.lock().unwrap().len(), 2);
        assert!(
            dispatch::OwnerTurn::probes(CHANNEL).1 > held_late_reads,
            "the retry uses actual execute_owned again"
        );
        assert!(
            resolved(&session, &path, fx.logical(), &nonce),
            "the existing Idle path resolves the clear Pending"
        );
        assert_eq!(final_state["items"], json!([]), "{final_state}");
        assert!(final_state["head_injection"].is_null(), "{final_state}");
        assert_eq!(final_state["retained_inputs"], json!([]), "{final_state}");
        assert_eq!(launcher.creates.load(Ordering::SeqCst), 1);
    });
}

#[test]
fn a_clear_discarded_late_held_original_never_returns_on_verified_clear_pg() {
    let fx = Fixture::new("busy-late-discard-entry", None);
    let launcher = Arc::new(Launcher::default());
    let (nonce, started) = reconnect::launch(&fx, &launcher);
    let before = fx.rig.sends().len();
    let _registry = fx.rig.registry_on_this_thread();
    let _admission = open_admission();
    let intake = Intake::new(&fx, &started);
    intake.runtime.block_on(async {
        let (drive, mut passes, original) = late_unknown(&fx, &intake, &started).await;
        let persistence = discord::queue_persistence_context(
            &intake.shared,
            &ProviderKind::Claude,
            ChannelId::new(CHANNEL),
        );
        let cleared = intake.mailbox.clear(persistence).await;
        assert!(
            cleared
                .discarded_message_ids
                .contains(&MessageId::new(original.message_id))
        );
        let path = awaited(&fx, &nonce, &uuid()).await;
        for _ in 0..2 {
            wake(drive.clone());
            assert_eq!(
                next_pass(&mut passes).await,
                None,
                "there is no front to retry"
            );
        }
        assert!(
            tokio::time::timeout(Duration::from_secs(5), passes.recv())
                .await
                .is_err(),
            "the obsolete held watch revives nothing"
        );
        assert!(!path.exists());
        assert_eq!(fx.rig.sends().len(), before);
        assert_eq!(state(&intake).await["items"], json!([]));
    });
}

#[test]
fn an_unknown_kept_herdr_original_never_returns_on_verified_clear_pg() {
    let fx = Fixture::new("busy-unknown-keep-entry", None);
    let launcher = Arc::new(Launcher::default());
    let (nonce, started) = reconnect::launch(&fx, &launcher);
    let before = fx.rig.sends().len();
    let _registry = fx.rig.registry_on_this_thread();
    let _admission = open_admission();
    let intake = Intake::new(&fx, &started);
    intake.runtime.block_on(async {
        let original = queued(&intake).await;
        let (drive, mut passes) = actual_drive(&intake);
        fx.rig.answer_sends_then_leave_unanswered(1);
        wake(drive.clone());
        let first = next_pass(&mut passes)
            .await
            .expect("an actual idle submit ran");
        assert!(
            first.is_err(),
            "the lost Enter reply is uncertain: {first:?}"
        );
        assert_eq!(fx.rig.sends().len() - before, 2);
        fx.rig.leave_sends_unanswered(false);
        let now = chrono::Utc::now().timestamp_millis().max(0) as u64 + 31_000;
        intake
            .mailbox
            .v2_request(move |slot| {
                let owner = slot.as_mut().ok_or("no owner")?;
                owner.tick(now)?;
                assert!(
                    owner.requeue_known_not_sent()?.is_empty(),
                    "Unknown is not proven unsent"
                );
                Ok(())
            })
            .await
            .unwrap();
        let kept = state(&intake).await;
        assert_eq!(kept["items"], json!([]), "{kept}");
        assert!(kept["head_injection"].is_null(), "{kept}");
        assert_eq!(
            kept["retained_inputs"].as_array().unwrap().len(),
            1,
            "{kept}"
        );
        assert_eq!(
            kept["retained_inputs"][0]["input"]["original"]["message_id"],
            original.message_id
        );
        let path = awaited(&fx, &nonce, &uuid()).await;
        for _ in 0..2 {
            wake(drive.clone());
            assert_eq!(next_pass(&mut passes).await, None);
        }
        assert!(!path.exists());
        assert_eq!(
            fx.rig.sends().len() - before,
            2,
            "a possibly keyed input is never replayed"
        );
        assert_eq!(state(&intake).await["items"], json!([]));
    });
}

// Append inside busy_entry_tests::late_tests after applying the canonical retry observer proposal.
#[test]
fn a_verified_clear_waits_for_its_held_attempts_own_unwind_pg() {
    let fx = Fixture::new("busy-held-own-unwind", None);
    let launcher = Arc::new(Launcher::default());
    let (nonce, started) = reconnect::launch(&fx, &launcher);
    let before = fx.rig.sends().len();
    let _registry = fx.rig.registry_on_this_thread();
    let _admission = open_admission();
    let intake = Intake::new(&fx, &started);
    intake.runtime.block_on(async {
        let original = queued(&intake).await;
        let (drive, mut passes) = actual_drive(&intake);
        let (entered, release) = dispatch::OwnerTurn::pause_late(CHANNEL);
        wake(drive.clone());
        entered
            .recv_timeout(LIMIT)
            .expect("the actual late entry pauses");
        let token = intake
            .mailbox
            .cancel_token()
            .await
            .unwrap()
            .expect("this attempt's token");
        let row = inflight::load_inflight_state_read_only(&ProviderKind::Claude, CHANNEL)
            .expect("this attempt's actual inflight row");
        assert_eq!(row.user_msg_id, original.message_id);
        assert_eq!(row.turn_nonce.as_deref(), token.turn_nonce());
        unknown_source(&started);
        release.send(()).unwrap();
        let held = next_pass(&mut passes)
            .await
            .expect("the held actual input settles locally");
        assert!(held.is_err_and(|error| error.starts_with("NotSent(")));
        assert_eq!(fx.rig.sends().len(), before);
        assert!(
            host::channel_accepts(CHANNEL),
            "the held attempt still has its O observer"
        );
        tokio::time::timeout(LIMIT, async {
            loop {
                if intake.mailbox.has_active_turn().await == Ok(false)
                    && inflight::load_inflight_state_read_only(&ProviderKind::Claude, CHANNEL)
                        .is_none()
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the actual held attempt finishes its initial cleanup");
        let session = uuid();
        let path = crate::services::claude_tui::transcript_tail::claude_transcript_path(
            fx.cwd.path(),
            &session,
            None,
        )
        .unwrap();
        let (retried, release) = dispatch::OwnerTurn::pause_late(CHANNEL);
        let heads = write_on_enter(&fx, &intake, &session, &path, true);
        assert_eq!(awaited(&fx, &nonce, &session).await, path);
        // Hold the captured episode after clear's own cleanup, so that cleanup cannot release it.
        intake
            .mailbox
            .restore_active_turn(
                token.clone(),
                UserId::new(original.author_id),
                MessageId::new(original.message_id),
            )
            .await;
        inflight::save_inflight_state(&row).unwrap();
        assert_eq!(intake.mailbox.has_active_turn().await, Ok(true));
        assert!(
            inflight::load_inflight_state_read_only(&ProviderKind::Claude, CHANNEL)
                .is_some_and(|live| live.turn_nonce == row.turn_nonce)
        );
        let since = std::time::Instant::now();
        for _ in 0..2 {
            let (_, observed) =
                crate::services::discord::queue_io::input_owner::held_watch::watch_observed(
                    drive.clone(),
                    original.message_id,
                );
            let read = tokio::time::timeout(LIMIT, observed)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                read.verdict,
                crate::services::discord::queue_io::input_owner::held_watch::Verdict::Keep,
                "a ready clear cannot spend its wake on its own unfinished attempt"
            );
        }
        assert!(since.elapsed() >= Duration::from_secs(2));
        assert_eq!(intake.mailbox.has_active_turn().await, Ok(true));
        assert!(
            inflight::load_inflight_state_read_only(&ProviderKind::Claude, CHANNEL)
                .is_some_and(|live| live.turn_nonce == row.turn_nonce)
        );
        assert!(retried.try_recv().is_err());
        assert!(!path.exists());
        assert_eq!(fx.rig.sends().len(), before);
        let persistence = discord::queue_persistence_context(
            &intake.shared,
            &ProviderKind::Claude,
            ChannelId::new(CHANNEL),
        );
        let _ = intake
            .mailbox
            .finish_turn_if_token(Some(token), persistence)
            .await;
        assert_eq!(intake.mailbox.has_active_turn().await, Ok(false));
        assert!(inflight::clear_inflight_state(
            &ProviderKind::Claude,
            CHANNEL
        ));
        // The same watch, without another queue wake, now admits the exact front.
        retried
            .recv_timeout(LIMIT)
            .expect("own unwind releases the pending source wake");
        let changes = subscribe_binding_events(CHANNEL).unwrap();
        release.send(()).unwrap();
        let settled =
            resolved_and_settled(&intake, &fx, &session, &path, &nonce, &heads, changes).await;
        assert_eq!(settled["items"], json!([]));
        assert!(settled["head_injection"].is_null());
        assert_eq!(fx.rig.sends().len() - before, 2);
        assert_eq!(launcher.creates.load(Ordering::SeqCst), 1);
    });
}
