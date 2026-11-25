//! A synthetic start (a wake-watch resume) on a Bound Herdr execution, through the actual idle
//! start: a proven-ended execution (missing pane or lone recorded root shell) is retired before any
//! write and a fresh one takes the input; one this owner can never recover is refused with a Failed
//! receipt; a live, unreadable or turn-held execution is left as it was. A person's input on a lone
//! root shell reaches the same recovery.
use super::missing_input::recording_runtime;
use super::*;
use crate::services::discord::queue_io::input_owner::admission::admit_with_context;
use crate::services::discord::queue_io::input_owner::held_watch;
use crate::services::discord::queue_io::input_owner::origin::InputOrigin;
use crate::services::discord::router::message_handler::provider_dispatch as dispatch;
use crate::services::discord::shared_state::test_rest;
use crate::services::session_host::herdr_socket_rig_tests::SHELL;
use crate::services::turn_orchestrator::{Waiting, waiting_origin};
use axum::http::StatusCode;
use std::sync::atomic::{AtomicBool, Ordering};

const SYNTHETIC: u64 = MESSAGE + 7;
const ORIGIN: &str = "i8422-wake-watch-origin";
const START_WAITS: &str = "NotSent(\"synthetic_waits_idle\")";
const REFUSED: &str = "NotSent(\"hosted_recovery_refused\")";
const DISCARDED: &str = "이 입력을 폐기했습니다";

/// What holds the channel before the start is judged.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Holds {
    Nothing,
    /// A managed turn's token.
    Token,
    /// The channel transition lock, as a registering start holds it.
    Lock,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Pane {
    Missing,
    Shell,
    Unreadable,
}

/// The recorded execution's pane as every later read sees it.
fn show(fx: &Fixture, pane: Pane) {
    match pane {
        Pane::Missing => {
            fx.rig.answer("pane.process_info", json!({"type":"ok"}));
            fx.rig.show_panes(&[]);
        }
        // The provider exited and left its recorded root shell alone in the pane.
        Pane::Shell => {
            fx.rig.show_panes(&[PANE]);
            fx.rig.foreground(&[SHELL]);
        }
        Pane::Unreadable => {
            fx.rig.answer("pane.process_info", json!({"type":"ok"}));
            fx.rig.answer("session.snapshot", json!({"type":"ok"}));
        }
    }
}

/// A wake-watch resume admitted as an HttpStart carrying its origin, granted as the owner's front.
async fn synthetic_front(intake: &Intake) -> OwnerFront {
    let original: PendingQueueItem = serde_json::from_value(json!({
        "author_id":7, "message_id":SYNTHETIC, "channel_id":CHANNEL,
        "source_message_ids":[SYNTHETIC], "queued_generation":1, "text":INPUT
    }))
    .unwrap();
    let context = json!({"origin":"headless","turn_id":format!("headless:{SYNTHETIC}"),
        "request_owner":7,"request_owner_name":"owner","source":"wake-watch",
        "metadata":{"human_input":{"origin_id":ORIGIN}}});
    assert_eq!(
        InputOrigin::from_context(Some(&context)),
        InputOrigin::HttpStart
    );
    let admitted = admit_with_context(&intake.mailbox, &original, AdmissionTicket::new(), context);
    assert!(matches!(admitted.await, OwnerAdmission::Accepted(_)));
    intake
        .mailbox
        .v2_request(|slot| {
            let actor = slot.as_mut().ok_or("no owner")?;
            let front = actor.front().ok_or("no front")?;
            actor.grant_front(&front)?;
            Ok(front)
        })
        .await
        .unwrap()
}

fn bound(fx: &Fixture, nonce: &str) -> bool {
    matches!(fx.record(), HostedRecord::Known(record)
        if record.state == HostedState::Bound && record.execution_nonce == nonce)
}

async fn hosted(pool: &sqlx::PgPool) -> HostedRecord {
    let raw: Value =
        sqlx::query_scalar("SELECT hosted_execution FROM sessions WHERE channel_id=$1")
            .bind(CHANNEL.to_string())
            .fetch_one(pool)
            .await
            .unwrap();
    HostedRecord::decode(Some(&raw))
}

fn queue(intake: &Intake) -> Value {
    intake
        .runtime
        .block_on(
            intake
                .mailbox
                .v2_request(|slot| slot.as_ref().unwrap().json()),
        )
        .unwrap()
}

/// One synthetic start through the owner's actual idle start against `pane`; `foreign` moves the
/// row to another endpoint and `holds` takes the channel first.
fn synthetic_case(
    tag: &str,
    (pane, foreign, holds): (Pane, bool, Holds),
    check: impl FnOnce(&Fixture, &Intake, &str, Result<(), String>),
) {
    let fx = Fixture::new(tag, None);
    let launcher = Arc::new(Launcher::default());
    let (nonce, started) = reconnect::launch(&fx, &launcher);
    let _registry = fx.rig.registry_on_this_thread();
    let _admission = open_admission();
    let _gate = discord::router::intake_gate::busy_inject::test_support::open_gate(CHANNEL);
    let mut intake = Intake::new(&fx, &started);
    let _writer = crate::services::herdr_launch::force_writer_accepts(Some(true));
    let original_runtime = recording_runtime(&mut intake, &fx, false);
    show(&fx, pane);
    if foreign {
        fx.rt.block_on(async {
            sqlx::query("UPDATE sessions SET hosted_execution=jsonb_set(hosted_execution,'{location,named_session}', to_jsonb('foreign-session'::text)) WHERE channel_id=$1")
                .bind(CHANNEL.to_string())
                .execute(&fx.pool)
                .await
                .unwrap();
        });
    }
    let result = intake.runtime.block_on(async {
        if holds == Holds::Token {
            let token = Arc::new(CancelToken::new());
            let holder = MessageId::new(MESSAGE - 1);
            intake
                .mailbox
                .restore_active_turn(token, UserId::new(7), holder)
                .await;
        }
        let lock = intake
            .shared
            .session_transition_locks
            .lock(ChannelId::new(CHANNEL));
        let _held = (holds == Holds::Lock).then(|| lock.try_lock_owned().unwrap());
        let front = synthetic_front(&intake).await;
        let start =
            dispatch::start_idle_for_test(intake.shared.clone(), ChannelId::new(CHANNEL), front);
        tokio::time::timeout(LIMIT, start).await.unwrap()
    });
    eprintln!("I8422_SYNTHETIC_RESULT {tag} {result:?}");
    check(&fx, &intake, &nonce, result);
    drop(intake);
    drop(original_runtime);
}

// T1c · T2c: a proven-ended execution is retired before any write; the start no longer waits.
#[test]
fn a_synthetic_start_retires_a_dead_bound_execution_before_any_write_pg() {
    for (tag, pane) in [
        ("dead-bound-missing", Pane::Missing),
        ("dead-bound-shell", Pane::Shell),
    ] {
        synthetic_case(
            tag,
            (pane, false, Holds::Nothing),
            |fx, _, nonce, result| {
                assert_ne!(
                    result.as_ref().err().map(String::as_str),
                    Some(START_WAITS),
                    "{pane:?}: the start must not wait on a dead execution"
                );
                assert!(!bound(fx, nonce), "{pane:?}: the old execution was retired");
                assert!(
                    !held_watch::has_watch(CHANNEL),
                    "{pane:?}: nothing is watched"
                );
                assert_eq!(
                    fx.rig.sends().len(),
                    2,
                    "{pane:?}: no input reached the old pane"
                );
            },
        );
    }
}

// T4c · T5 · T11a: a live but unreadable execution, one a turn token holds, or one whose transition
// lock a registering start took first, is never retired (CAS 0).
#[test]
fn a_synthetic_start_leaves_an_unreadable_or_held_execution_bound_pg() {
    for (tag, pane, holds) in [
        ("dead-bound-unreadable", Pane::Unreadable, Holds::Nothing),
        ("dead-bound-token", Pane::Missing, Holds::Token),
        ("dead-bound-lock", Pane::Missing, Holds::Lock),
    ] {
        synthetic_case(tag, (pane, false, holds), |fx, intake, nonce, result| {
            assert_eq!(
                result.as_ref().err().map(String::as_str),
                Some(START_WAITS),
                "{tag}: {result:?}"
            );
            assert!(bound(fx, nonce), "{tag}: the execution stays Bound");
            let state = queue(intake);
            assert_eq!(
                state["items"].as_array().map(Vec::len),
                Some(1),
                "{tag}: kept"
            );
            assert_eq!(fx.rig.sends().len(), 2, "{tag}: nothing was written");
        });
    }
}

// T7: an execution this owner can never recover refuses the start with one reason and a Failed
// receipt for its origin; nothing reaches the provider and nothing stays queued.
#[test]
fn a_synthetic_start_on_a_foreign_execution_is_refused_with_a_failed_receipt_pg() {
    synthetic_case(
        "dead-bound-foreign",
        (Pane::Missing, true, Holds::Nothing),
        |fx, intake, nonce, result| {
            assert_eq!(result, Err(REFUSED.to_string()));
            assert!(bound(fx, nonce), "a foreign execution is never retired");
            let state = queue(intake);
            assert!(state["items"].as_array().unwrap().is_empty(), "{state}");
            assert!(state["retained_inputs"].as_array().unwrap().is_empty());
            let owed = &state["owed_drop_receipts"];
            assert_eq!(owed.as_array().map(Vec::len), Some(1), "{owed}");
            assert_eq!(owed[0]["outcome"], "failed", "{owed}");
            assert_eq!(owed[0]["origins"], json!([ORIGIN]), "{owed}");
            let waiting = waiting_origin("claude", CHANNEL, ORIGIN);
            assert!(
                matches!(&waiting, Ok(Some(Waiting::Owed(owed)))
                    if owed.outcome == crate::db::turns::TerminalOutcome::Failed),
                "the queue file a restart reads keeps it Failed: {waiting:?}"
            );
            let calls = intake._api.take();
            assert_eq!(
                calls.iter().filter(|call| call.contains(DISCARDED)).count(),
                1,
                "{calls:?}"
            );
            assert_eq!(fx.rig.sends().len(), 2, "no write");
        },
    );
}

// T9c: a start watched while its execution could not be read is woken once that execution ends;
// the look itself retires the proven-ended row.
#[test]
fn a_watched_synthetic_start_wakes_once_its_execution_is_proven_ended_pg() {
    synthetic_case(
        "dead-bound-watch",
        (Pane::Unreadable, false, Holds::Nothing),
        |fx, intake, nonce, result| {
            assert_eq!(result.as_ref().err().map(String::as_str), Some(START_WAITS));
            assert!(
                held_watch::armed(CHANNEL),
                "the unreadable start armed its watch"
            );
            show(fx, Pane::Missing);
            let woken = Arc::new(AtomicBool::new(false));
            let seen = woken.clone();
            let drive = Arc::new(dispatch_drive(intake, seen));
            let observed = intake.runtime.block_on(async {
                let (_, observed) = held_watch::watch_observed(drive, SYNTHETIC);
                tokio::time::timeout(LIMIT, observed)
                    .await
                    .unwrap()
                    .unwrap()
            });
            assert_eq!(observed.verdict, held_watch::Verdict::Wake);
            assert!(!bound(fx, nonce), "the look retired the ended execution");
            assert_eq!(fx.rig.sends().len(), 2, "no write before the woken start");
        },
    );
}

fn dispatch_drive(
    intake: &Intake,
    woken: Arc<AtomicBool>,
) -> crate::services::discord::queue_io::input_owner::drive::Drive {
    crate::services::discord::queue_io::input_owner::drive::Drive {
        mailbox: intake.mailbox.clone(),
        channel: CHANNEL,
        handoff: Arc::new(move |_| {
            woken.store(true, Ordering::SeqCst);
            Box::pin(async { Err("woken start judged again elsewhere".into()) })
        }),
        settled: None,
    }
}

// T10b: a person's input on a lone recorded root shell now reaches the same recovery under its own
// borrowed intake guard; #8212 alone accepted only a missing pane.
#[test]
fn a_person_input_retires_a_lone_root_shell_under_its_own_guard_pg() {
    let fx = Fixture::new("dead-bound-person-shell", None);
    let launcher = Arc::new(Launcher::default());
    let (nonce, started) = reconnect::launch(&fx, &launcher);
    let _registry = fx.rig.registry_on_this_thread();
    let _admission = open_admission();
    let _gate = discord::router::intake_gate::busy_inject::test_support::open_gate(CHANNEL);
    let mut intake = Intake::new(&fx, &started);
    let _writer = crate::services::herdr_launch::force_writer_accepts(Some(true));
    let original_runtime = recording_runtime(&mut intake, &fx, false);
    show(&fx, Pane::Shell);
    let result = intake.runtime.block_on(async {
        let front = intake.front().await;
        let (entered, resume) =
            discord::router::intake_dispatch::after_upload_take(ChannelId::new(CHANNEL))
                .await
                .pause();
        let start =
            dispatch::start_idle_for_test(intake.shared.clone(), ChannelId::new(CHANNEL), front);
        let record = async {
            tokio::time::timeout(LIMIT, entered).await.unwrap().unwrap();
            let guard = test_rest::install(intake._api.http.clone());
            resume.send(()).unwrap();
            guard
        };
        let (result, _rest) = tokio::join!(tokio::time::timeout(LIMIT, start), record);
        result.unwrap()
    });
    eprintln!("I8422_PERSON_SHELL_RESULT {result:?}");
    assert!(
        !bound(&fx, &nonce),
        "the lone root shell's execution was retired"
    );
    assert_eq!(fx.rig.sends().len(), 2, "no input reached the old pane");
    drop(intake);
    drop(original_runtime);
}

// T1c delivery: the wake-watch resume itself, through the HTTP start, retires the missing pane's
// execution, launches a fresh one and delivers once.
#[test]
fn a_dead_bound_wake_watch_start_launches_fresh_and_delivers_once_pg() {
    let fx = Fixture::new("dead-bound-cold-delivery", None);
    let launcher = Arc::new(Launcher::default());
    let (old_nonce, previous) = reconnect::launch(&fx, &launcher);
    let _registry = fx.rig.registry_on_this_thread();
    let _admission = open_admission();
    let _gate = discord::router::intake_gate::busy_inject::test_support::open_gate(CHANNEL);
    let mut intake = Intake::new(&fx, &previous);
    let _writer = crate::services::herdr_launch::force_writer_accepts(Some(true));
    let original_runtime = recording_runtime(&mut intake, &fx, true);
    let _hook = crate::services::claude_tui::hook_server::publish_hook_endpoint(
        "http://127.0.0.1:1".into(),
    );
    fx.rig.answer(
        "ping",
        json!({"type":"pong","version":"0.9.3","protocol":22}),
    );
    fx.rig.answer("workspace.create",json!({"type":"workspace_created","root_pane":{"pane_id":PANE,"workspace_id":"w1","tab_id":"w1:1","revision":0,"cwd":fx.cwd.path()}}));
    show(&fx, Pane::Missing);
    let app = intake.runtime.block_on(async {
        intake
            .shared
            .core
            .lock()
            .await
            .sessions
            .get_mut(&ChannelId::new(CHANNEL))
            .unwrap()
            .session_id = None;
        super::http_start::app(&fx, &intake).await
    });
    let before = fx.rig.sends().len();
    let (command_sent, command_seen) = std::sync::mpsc::channel();
    let (continue_create, create_ready) = std::sync::mpsc::channel();
    let mut captured = false;
    fx.rig.on_send(move |_| {
        if !captured {
            captured = true;
            command_sent.send(()).unwrap();
            crate::test_support::wait::recv_within(&create_ready, LIMIT);
        }
    });
    let mut events = fx.ingress.state.subscribe();
    let (status, body) = intake.runtime.block_on(super::http_start::call_source(
        &app,
        "start",
        ORIGIN,
        "wake-watch",
    ));
    assert_eq!(status, StatusCode::OK, "{body}");
    let pending = fx
        .rt
        .block_on(async {
            tokio::time::timeout(LIMIT, async {
                loop {
                    if let HostedRecord::Known(record) = hosted(&fx.pool).await
                        && record.state == HostedState::Pending
                        && record.execution_nonce != old_nonce
                    {
                        break record;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
        })
        .expect("the retired channel reaches a fresh create");
    let context = crate::services::tui_prompt_dedupe::binding_context::execution_context(
        "claude",
        &pending.execution_nonce,
    )
    .unwrap();
    let session = context.expected_native_session_id.unwrap();
    let context_path = context_of(&pending.execution_nonce);
    crate::test_support::wait::recv_within(&command_seen, LIMIT);
    fx.rig.run_provider(&context_path, false);
    fx.rig.foreground(&[20]);
    fx.rig.show_panes(&[PANE]);
    continue_create.send(()).unwrap();
    fx.rt.block_on(async {
        tokio::time::timeout(LIMIT, async {
            while !matches!(hosted(&fx.pool).await, HostedRecord::Known(record)
                if record.execution_nonce == pending.execution_nonce && record.location.is_some())
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    });
    let path = crate::services::claude_tui::transcript_tail::claude_transcript_path(
        fx.cwd.path(),
        &session,
        fx.home().as_deref(),
    )
    .unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    if !path.exists() {
        std::fs::write(&path, b"").unwrap();
    }
    let envelope = HookBindingEnvelope::capture_from_env("claude", |name| {
        (name == "AGENTDESK_BINDING_CONTEXT").then(|| context_path.clone().into_os_string())
    })
    .encode()
    .unwrap();
    let started = Started {
        session,
        path,
        envelope,
    };
    assert_eq!(
        started.hook(
            &fx,
            "SessionStart",
            &json!({"session_id":started.session,"transcript_path":started.path,"source":"startup"})
        ),
        202
    );
    while let Ok(event) = events.try_recv() {
        crate::services::claude_tui::hook_server::publish_hook_event_for_tests(event);
    }
    fx.rt
        .block_on(async {
            tokio::time::timeout(LIMIT, async {
                while fx.rig.sends().len() < before + 3 {
                    tokio::task::yield_now().await;
                }
            })
            .await
        })
        .expect("the fresh pane takes the resume");
    fx.finished.store(false, Ordering::SeqCst);
    fx.answer_after(&started, before + 1);
    while let Ok(event) = events.try_recv() {
        crate::services::claude_tui::hook_server::publish_hook_event_for_tests(event);
    }
    intake.runtime.block_on(async {
        tokio::time::timeout(LIMIT, async {
            while intake.mailbox.has_active_turn().await != Ok(false) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the delivered resume settles");
    });
    let sends = fx.rig.sends();
    assert_eq!(sends.len(), before + 3, "{sends:?}");
    assert!(sends[before + 1]["text"].as_str().unwrap().contains(INPUT));
    assert_eq!(sends[before + 2]["keys"], json!(["enter"]));
    assert!(matches!(fx.record(), HostedRecord::Known(record)
            if record.state == HostedState::Bound && record.execution_nonce == pending.execution_nonce));
    assert!(
        !intake
            ._api
            .take()
            .iter()
            .any(|call| call.contains(DISCARDED))
    );
    drop(intake);
    drop(original_runtime);
}
