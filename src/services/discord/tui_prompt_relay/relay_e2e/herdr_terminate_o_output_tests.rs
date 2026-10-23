//! A forced HTTP cancel held inside the Herdr channel's termination leaves both output owners
//! posting: O delivers the execution's sealed answer and a Legacy thread answers before it ends.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::StatusCode;
use poise::serenity_prelude::ChannelId;
use serde_json::{Value, json};

use super::super::{
    OHost, Terminating, WAIT, append, assistant, begin_era, bind_source, posted_in_ledger, root,
    wait_until,
};
use crate::services::discord::health::HealthRegistry;
use crate::services::discord::herdr_terminate::probe_before_postlude;
use crate::services::discord::rebind_channel_session;
use crate::services::discord::tui_prompt_relay::relay_e2e::{
    CHANNEL_ID, SESSION_UUID, discord_mock,
};
use crate::services::provider::ProviderKind;
use crate::services::session_host::herdr_termination_rig as herdr;
use crate::services::tui_o::shadow::binding_reader::source_id_for;
use crate::services::tui_o::writer::WriterAlarm;
use crate::services::tui_prompt_dedupe::binding_events;

/// Where the cancel is held while both owners post.
const HOLDS: [&str; 2] = ["end-check", "postlude"];
const AUTH: &str = "c1-output";
const BEFORE: &str = "answered before the termination";
const SEALED: &str = "the sealed answer O delivers while the cancel is held";
const AFTER: &str = "the writer's own output after the cancel";
const THREAD: u64 = 940_487_400_000_771;
/// The Legacy provider stand-in's answer.
const LEGACY: &str = "ok";

/// The rig in its own child on a current-thread runtime, so the cancel's probes run on it; every
/// runtime thread logs bindings under the row's root.
fn output_child(path: &str) {
    if !herdr::in_child(path, CHANNEL_ID) {
        return;
    }
    let rig = herdr::P11TerminationRig::boot(CHANNEL_ID);
    let events = tempfile::tempdir().unwrap();
    let current: Arc<Mutex<PathBuf>> = Arc::new(Mutex::new(events.path().to_path_buf()));
    let (rig_install, row_root) = (rig.thread_installer(), current.clone());
    let install = Arc::new(move || {
        rig_install();
        binding_events::set_test_root(Some(&row_root.lock().unwrap()));
    });
    install();
    let on_start = Arc::clone(&install);
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .thread_keep_alive(Duration::from_millis(1))
        .on_thread_start(move || on_start())
        .build()
        .unwrap()
        .block_on(rows(rig, current, install));
}

/// The forced cancel through the production route, on the harness's runtime.
fn cancel(run: &Terminating) -> impl std::future::Future<Output = (StatusCode, Value)> + use<> {
    let (pool, shared) = (run.pool.clone(), run.harness.shared.clone());
    async move {
        let registry = Arc::new(HealthRegistry::new());
        registry.register("claude".into(), shared).await;
        let mut config = crate::config::Config::default();
        config.server.auth_token = Some(AUTH.into());
        let app = crate::server::routes::home_stop_test_app(
            config,
            Some(pool),
            Some(registry),
            herdr::NODE,
        );
        let bearer = format!("Bearer {AUTH}");
        let headers = [
            ("authorization", bearer.as_str()),
            ("content-type", "application/json"),
        ];
        let uri = format!("/turns/{CHANNEL_ID}/cancel");
        let body = json!({"force": true}).to_string();
        crate::server::routes::home_stop_test_post(&app, &uri, &headers, &body).await
    }
}

/// How many messages the Discord mock holds with `content`, in any channel.
fn legacy_posts(run: &Terminating, content: &str) -> usize {
    let messages = run.harness.messages();
    messages.iter().filter(|(_, text)| text == content).count()
}

/// What both owners had delivered when the cancel was still held.
struct DuringHold {
    o_posts: Vec<String>,
    sealed_in_ledger: usize,
    legacy_answers: usize,
    legacy_completed: bool,
}

async fn rows(
    rig: herdr::P11TerminationRig,
    events: Arc<Mutex<PathBuf>>,
    install: Arc<dyn Fn() + Send + Sync>,
) {
    let mut ran = 0;
    for hold in HOLDS {
        let row_events = tempfile::tempdir().unwrap();
        *events.lock().unwrap() = row_events.path().to_path_buf();
        install();
        let run = Terminating::bind(&rig).await;
        // The runtime's Discord transport is cached as in production, so any post would land.
        run.harness.cache_relay_transport();
        sqlx::query("UPDATE sessions SET status = 'turn_active', last_heartbeat = NOW() WHERE session_key = $1")
            .bind(&run.session_key).execute(&run.pool).await.unwrap();
        let a_session = uuid::Uuid::new_v4().to_string();
        let a_path = root().join("a.jsonl");
        std::fs::write(&a_path, b"").unwrap();
        bind_source(&run.logical, &a_session, &a_path);
        let a_source = source_id_for(&a_session, &a_path).unwrap();
        begin_era(&a_source, "c1-output");
        let mut o = OHost::new();
        o.start();
        append(&a_path, &assistant("m1", BEFORE));
        o.posted(&[BEFORE]).await;
        // The answer is sealed while the writer is down, so only the held window can deliver it.
        o.stop().await;
        append(&a_path, &assistant("m2", SEALED));
        run.harness.add_thread(THREAD);
        // The thread is bound like the channel, so its Legacy turn runs the provider stub.
        let cwd = run.harness.root.path().to_str().unwrap().to_string();
        let (shared, thread) = (&run.harness.shared, ChannelId::new(THREAD));
        rebind_channel_session(shared, &ProviderKind::Claude, thread, &cwd, SESSION_UUID).await;
        let mut completions = run.harness.subscribe_completions();

        let (reached, release) = (
            Arc::new(tokio::sync::Notify::new()),
            Arc::new(tokio::sync::Notify::new()),
        );
        let mut pause = None;
        match hold {
            "end-check" => pause = Some(rig.pause_end_check()),
            _ => {
                let (probe_reached, probe_release) = (reached.clone(), release.clone());
                probe_before_postlude(Box::new(move || {
                    Box::pin(async move {
                        let released = probe_release.notified();
                        probe_reached.notify_one();
                        released.await;
                    })
                }));
            }
        }
        let request = cancel(&run);
        let observe = async {
            match pause.as_mut() {
                Some(pause) => (&mut pause.arrived)
                    .await
                    .expect("the end check was reached"),
                None => tokio::time::timeout(WAIT, reached.notified())
                    .await
                    .expect("hang guard: the postlude was reached"),
            }
            o.start();
            let delivered = o.until(|posts| posts.len() >= 2).await;
            let mut arrival = discord_mock::user_message(THREAD + 1, "a Legacy thread question");
            arrival.channel_id = ChannelId::new(THREAD);
            let intake = run.harness.spawn_message(arrival);
            let answered = wait_until(WAIT, || {
                let answers = legacy_posts(&run, LEGACY);
                Box::pin(async move { answers >= 1 })
            })
            .await;
            let completed = tokio::time::timeout(WAIT, async {
                loop {
                    match completions.recv().await {
                        Ok(event) if event.channel_id.get() == THREAD => return true,
                        Ok(_) => continue,
                        Err(_) => return false,
                    }
                }
            });
            let legacy_completed = completed.await.unwrap_or(false);
            assert!(
                delivered,
                "{hold}: O delivered while held: {:?} {:?}",
                o.posts(),
                o.halts()
            );
            assert!(
                answered,
                "{hold}: the Legacy thread answered while held: {:?}",
                run.harness.messages()
            );
            let during = DuringHold {
                o_posts: o.posts(),
                sealed_in_ledger: posted_in_ledger(SEALED),
                legacy_answers: legacy_posts(&run, LEGACY),
                legacy_completed,
            };
            intake.await.unwrap().unwrap();
            match pause.take() {
                Some(pause) => pause.release.send(()).unwrap(),
                None => release.notify_one(),
            }
            during
        };
        let ((status, reply), during) = tokio::join!(request, observe);

        // Before the release: each owner had delivered once, through its own transport.
        assert_eq!(during.o_posts, [BEFORE, SEALED], "{hold}");
        assert_eq!(
            during.sealed_in_ledger, 1,
            "{hold}: O's ledger records the delivery"
        );
        assert_eq!(during.legacy_answers, 1, "{hold}: one Legacy answer");
        assert!(during.legacy_completed, "{hold}: the Legacy turn completed");
        // After it: the cancel succeeded, posted nothing of either owner, and O still owns output.
        assert_eq!(status, StatusCode::OK, "{hold}: {reply}");
        let body = reply.to_string();
        assert!(
            !body.contains(SEALED) && !body.contains(BEFORE),
            "{hold}: {reply}"
        );
        append(&a_path, &assistant("m3", AFTER));
        let reached_after = o.until(|posts| posts.len() >= 3).await;
        o.stop().await;
        assert!(reached_after, "{hold}: {:?} {:?}", o.posts(), o.halts());
        assert_eq!(
            o.posts(),
            [BEFORE, SEALED, AFTER],
            "{hold}: no output twice"
        );
        assert_eq!(posted_in_ledger(SEALED), 1, "{hold}");
        assert_eq!(o.halts(), Vec::<WriterAlarm>::new(), "{hold}");
        assert_eq!(
            legacy_posts(&run, LEGACY),
            1,
            "{hold}: no Legacy answer twice"
        );
        for text in [BEFORE, SEALED, AFTER] {
            assert_eq!(
                legacy_posts(&run, text),
                0,
                "{hold}: O's output is O's alone"
            );
        }
        run.finish().await;
        drop(row_events);
        ran += 1;
    }
    assert_eq!(ran, HOLDS.len());
}

// A forced HTTP cancel held at its end check or before its postlude: before the release O posts
// the sealed answer and records it, and a Legacy thread answers; afterwards nothing posts twice.
#[test]
fn force_actual_o_and_legacy_output_during_termination_pg() {
    output_child(concat!(
        module_path!(),
        "::force_actual_o_and_legacy_output_during_termination_pg"
    ));
}
