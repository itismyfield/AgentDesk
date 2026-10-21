//! The first complete timestamp record is the live reader's admitted start, even when metadata
//! exists before that record; settlement must admit the next actor and its real pane input.

use super::live_end::LiveEnd;
use super::*;
use poise::serenity_prelude::{ChannelId, MessageId, UserId};

#[derive(Clone, Copy, Debug)]
enum Source {
    Fresh,
    RestoredSelector,
    Warm,
    Cleared,
}

fn timestamped(mut value: Value, at: chrono::DateTime<chrono::Utc>) -> Value {
    value["timestamp"] = at.to_rfc3339().into();
    value
}

fn records(session: &str, at: chrono::DateTime<chrono::Utc>) -> [Value; 3] {
    [
        timestamped(
            json!({"type":"user", "sessionId":session,
            "message":{"role":"user", "content":"질문"}}),
            at,
        ),
        timestamped(
            json!({"type":"assistant", "sessionId":session,
            "message":{"role":"assistant", "content":[{"type":"text", "text":"답"}]}}),
            at,
        ),
        timestamped(
            json!({"type":"system", "subtype":"stop_hook_summary", "sessionId":session,
            "hookCount":1,"hasOutput":true}),
            at,
        ),
    ]
}

/// The Enter acknowledgement creates only metadata. The reader's scan then blocks at its poll
/// latch until this test writes the prompt and output, so scheduler timing cannot select the start.
struct Window {
    observed: std::sync::mpsc::Receiver<()>,
    release: std::sync::mpsc::Sender<()>,
}

impl Window {
    fn prepare(fx: &Fixture, session: &str, path: &Path) -> Self {
        let (polled, observed) = std::sync::mpsc::channel();
        let (release, wait) = std::sync::mpsc::channel();
        crate::services::claude::herdr_turn::start_offset::test_hooks::on_poll_once(
            path,
            Box::new(move || {
                polled.send(()).unwrap();
                wait.recv_timeout(LIMIT)
                    .expect("the provider releases the timestamp-record scan");
            }),
        );
        let meta = json!({"type":"system", "subtype":"init", "sessionId":session});
        let written = path.to_owned();
        fx.rig.on_send(move |params| {
            if params["keys"] == json!(["enter"]) {
                append(&written, std::slice::from_ref(&meta));
            }
        });
        Self { observed, release }
    }

    fn finish(self, fx: &Fixture, session: &str, path: &Path) -> (LiveEnd, Arc<CancelToken>, u64) {
        self.observed
            .recv_timeout(LIMIT)
            .expect("the reader scanned the metadata-only file");
        let expected_start = std::fs::metadata(path).unwrap().len();
        let token = fx.cancel.lock().unwrap().clone();
        let url = &fx.db.as_ref().unwrap().database_url;
        let live = LiveEnd::start(
            url,
            TOKEN,
            (ProviderKind::Claude, CHANNEL, fx.logical()),
            &token,
        );
        append(path, &records(session, chrono::Utc::now()));
        self.release.send(()).unwrap();
        (live, token, expected_start)
    }
}

fn first_turn(
    fx: &Fixture,
    launcher: &Arc<Launcher>,
    source: Source,
) -> (
    Vec<StreamMessage>,
    LiveEnd,
    Arc<CancelToken>,
    String,
    PathBuf,
    u64,
) {
    let mut live = None;
    let mut session_path = None;
    let row = match source {
        Source::Warm => {
            let row = fx.store_bound();
            let binding =
                crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(fx.logical())
                    .unwrap();
            let session = binding.session_id.unwrap();
            let path = PathBuf::from(binding.output_path);
            append(
                &path,
                &records(&session, chrono::Utc::now() - chrono::Duration::seconds(5)),
            );
            super::live_end::log_source(fx);
            session_path = Some((session, path));
            row
        }
        Source::Cleared => {
            // The real launch installs the execution marker and canonical fold that a clear uses.
            super::reconnect::launch(fx, launcher);
            let session = uuid();
            let path = super::reconnect::clear_pending(fx, &session);
            assert!(
                !path.exists(),
                "clear selects a new transcript before it exists"
            );
            session_path = Some((session, path));
            fx.record()
        }
        _ => HostedRecord::Legacy,
    };
    let bound = matches!(source, Source::Warm | Source::Cleared);
    let window = session_path
        .as_ref()
        .map(|(session, path)| Window::prepare(fx, session, path));
    let ports = fx.ports(launcher);
    let (result, messages) = fx.turn(&row, &ports, || {
        if !bound {
            let Some(started) = fx.start_provider(launcher, false) else {
                return;
            };
            let session = started.session.clone();
            let path = started.path.clone();
            let window = Window::prepare(fx, &session, &path);
            assert!(
                !path.exists(),
                "a fresh launch has no transcript before input"
            );
            let payload = json!({"session_id":session,"source":"startup","transcript_path":path});
            fx.started.store(true, Ordering::SeqCst);
            assert_eq!(started.hook(fx, "SessionStart", &payload), 202);
            live = Some(window.finish(fx, &session, &path));
            session_path = Some((session, path));
        } else {
            let (session, path) = session_path.as_ref().unwrap();
            live = Some(window.unwrap().finish(fx, session, path));
        }
    });
    assert_eq!(result, Ok(()), "{source:?}: {messages:?}");
    let (live, token, start) = live.expect("the turn entered the real reader");
    let (session, path) = session_path.unwrap();
    (messages, live, token, session, path, start)
}

fn run(source: Source) {
    let _ = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_test_writer()
        .with_max_level(tracing::Level::WARN)
        .try_init();
    let fx = Fixture::new(&format!("offset-{source:?}"), None);
    let launcher = Arc::new(Launcher::default());
    let restored_selector = matches!(source, Source::RestoredSelector).then(uuid);
    if let Some(restored) = restored_selector.as_ref() {
        fx.rt.block_on(async {
            sqlx::query("UPDATE sessions SET claude_session_id=$1, raw_provider_session_id=$1 WHERE channel_id=$2")
                .bind(restored).bind(CHANNEL.to_string()).execute(&fx.pool).await.unwrap();
            let ids = crate::db::dispatched_sessions::load_provider_session_ids_pg(
                &fx.pool, &session_key(&fx.owner), Some("claude"),
            ).await.unwrap().unwrap();
            assert_eq!(ids.raw_provider_session_id.as_deref(), Some(restored.as_str()));
            assert!(ids.claude_session_id.is_some(), "the DB-restored selector is resumed");
        });
    }
    let (messages, live, token, session, path, expected_start) = first_turn(&fx, &launcher, source);
    if let Some(restored) = restored_selector {
        assert_ne!(
            session, restored,
            "a restored selector still creates a fresh native Herdr session"
        );
        assert_eq!(launcher.creates.load(Ordering::SeqCst), 1);
    }
    let starts: Vec<_> = messages
        .iter()
        .filter_map(|message| match message {
            StreamMessage::ClaudeTuiTerminalDone { source_start, .. } => Some(*source_start),
            _ => None,
        })
        .collect();
    live.route();
    let _registry = fx.rig.registry_on_this_thread();
    let completed = live.bridge(&token, messages);
    let inflight = live.row().is_some();
    let held = live.rt.block_on(live.held());
    eprintln!(
        "START_OFFSET_CHAIN source={source:?} frame_start={starts:?} replay_start={expected_start} completion={completed:?} inflight={inflight} held={held}"
    );
    assert_eq!(
        completed,
        Some(crate::services::discord::turn_bridge::BridgeCompletionSignal::Finalized),
        "the real reader frame must pass admission and settle"
    );
    assert_eq!(
        starts,
        [expected_start],
        "{source:?}: actual reader start excludes timestamp-less metadata"
    );
    assert!(!inflight, "{source:?}: inflight released");
    assert!(!held, "{source:?}: mailbox token released");
    assert_eq!(
        live.wait_taken().len(),
        1,
        "settlement wakes the queued input once"
    );

    let before_next = fx.rig.sends().len();
    let next = Arc::new(CancelToken::new());
    assert!(
        live.rt
            .block_on(crate::services::discord::mailbox_try_start_turn(
                &live.shared,
                ChannelId::new(CHANNEL),
                next.clone(),
                UserId::new(7),
                MessageId::new(7_145_000_004),
            )),
        "the second intake actor is admitted without clear"
    );
    // The real next executor must submit both paste and Enter; dequeuing alone proves too little.
    fx.rig.on_send(|_| {});
    let (result, _) = fx.turn_on_token(&fx.record(), &fx.ports(&launcher), next, || {
        assert!(wait_for(&fx.finished, "second provider submission", || fx
            .rig
            .sends()
            .len()
            == before_next + 2));
        append(&path, &records(&session, chrono::Utc::now()));
    });
    assert_eq!(result, Ok(()));
    let sends = fx.rig.sends();
    assert_eq!(sends.len(), before_next + 2);
    assert_eq!(sends[before_next..], prompt_sends());
}

#[test]
fn a_fresh_metadata_window_admits_the_reader_and_second_provider_input_pg() {
    run(Source::Fresh);
}

#[test]
fn a_db_restored_selector_fresh_launch_admits_the_reader_and_second_input_pg() {
    run(Source::RestoredSelector);
}

#[test]
fn a_bound_warm_metadata_window_admits_the_reader_and_second_input_pg() {
    run(Source::Warm);
}

#[test]
fn a_clear_replacement_metadata_window_admits_the_reader_and_second_input_pg() {
    run(Source::Cleared);
}
