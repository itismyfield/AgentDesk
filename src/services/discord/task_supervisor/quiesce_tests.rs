use super::*;
use crate::services::discord::{ChannelId, MessageId, make_shared_data_for_tests};
use std::time::Duration;

const WAIT: Duration = Duration::from_secs(5);

fn spawn(
    cancel: Arc<AtomicBool>,
    future: impl Future<Output = ()> + Send + 'static,
) -> tokio::task::JoinHandle<()> {
    spawn_observed_tmux_watcher(
        "quiesce-test",
        make_shared_data_for_tests(),
        "quiesce-test".into(),
        cancel,
        future,
    )
}

async fn patch_ack(malformed_response: bool) {
    use axum::http::Method;
    use poise::serenity_prelude as serenity;
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let app = axum::Router::new().fallback(axum::routing::any({
        let entered = entered.clone();
        let release = release.clone();
        move |method: Method| {
            let entered = entered.clone();
            let release = release.clone();
            async move {
                assert_eq!(method, Method::PATCH);
                entered.notify_one();
                release.notified().await;
                axum::Json(if malformed_response { serde_json::json!({}) } else { serde_json::json!({
                    "id": "2", "channel_id": "1", "content": "settled",
                    "author": {"id": "3", "username": "test", "discriminator": "0001", "avatar": null},
                    "timestamp": "2026-09-27T00:00:00+00:00", "edited_timestamp": null, "tts": false,
                    "mention_everyone": false, "mentions": [], "mention_roles": [], "attachments": [],
                    "embeds": [], "pinned": false, "type": 0
                }) })
            }
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http = serenity::HttpBuilder::new("test-token")
        .proxy(format!("http://{}", listener.local_addr().unwrap()))
        .ratelimiter_disabled(true)
        .build();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let cancel = Arc::new(AtomicBool::new(false));
    let task = spawn(cancel.clone(), async move {
        let result = crate::services::discord::http::edit_channel_message(
            &http,
            ChannelId::new(1),
            MessageId::new(2),
            "settled",
        )
        .await;
        assert_eq!(result.is_err(), malformed_response);
    });
    tokio::time::timeout(WAIT, entered.notified())
        .await
        .unwrap();
    let request = quiesce(&cancel, WAIT);
    tokio::pin!(request);
    let early = tokio::time::timeout(Duration::from_millis(50), &mut request).await;
    release.notify_one();
    assert!(early.is_err(), "ACK arrived before PATCH settled");
    assert_eq!(
        request.await,
        if malformed_response {
            QuiesceAck::Ambiguous
        } else {
            QuiesceAck::Quiesced
        }
    );
    task.await.unwrap();
    server.abort();
}

#[tokio::test]
async fn quiesce_waits_for_inflight_patch() {
    patch_ack(false).await;
}

#[tokio::test]
async fn quiesce_reports_ambiguous_patch_response() {
    patch_ack(true).await;
}

#[tokio::test]
async fn quiesce_keeps_cancelled_transport_ambiguous_after_success() {
    use super::super::watcher_mutations::track_mutation;
    let cancel = Arc::new(AtomicBool::new(false));
    let (ready, started) = oneshot::channel();
    let (finish, resume) = oneshot::channel();
    let task = spawn(cancel.clone(), async {
        let pending = track_mutation(std::future::pending::<poise::serenity_prelude::Result<()>>());
        assert!(
            tokio::time::timeout(Duration::from_millis(1), pending)
                .await
                .is_err()
        );
        track_mutation(async { Ok(()) }).await.unwrap();
        ready.send(()).unwrap();
        resume.await.unwrap();
    });
    started.await.unwrap();
    let request = quiesce(&cancel, WAIT);
    tokio::pin!(request);
    assert!(futures::poll!(&mut request).is_pending());
    finish.send(()).unwrap();
    assert_eq!(request.await, QuiesceAck::Ambiguous);
    task.await.unwrap();
}

struct PausedReaderDrop {
    entered: Option<oneshot::Sender<()>>,
    release: std::sync::mpsc::Receiver<()>,
}
impl Drop for PausedReaderDrop {
    fn drop(&mut self) {
        let _ = self.entered.take().unwrap().send(());
        self.release.recv_timeout(WAIT).unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quiesce_ack_waits_for_reader_drop_and_registry_cleanup() {
    use crate::services::discord::tmux_watcher_registry::lock_tmux_watcher_registry;
    let cancel = Arc::new(AtomicBool::new(false));
    let (entered, dropping) = oneshot::channel();
    let (release, resume_drop) = std::sync::mpsc::channel();
    let (finish, resume) = oneshot::channel();
    let reader = PausedReaderDrop {
        entered: Some(entered),
        release: resume_drop,
    };
    let task = spawn(cancel.clone(), async {
        let _reader = reader;
        resume.await.unwrap();
    });
    let (locked, ready) = oneshot::channel();
    let (unlock, release_lock) = std::sync::mpsc::channel();
    let cleanup = std::thread::spawn(move || {
        let _registry = lock_tmux_watcher_registry();
        locked.send(()).unwrap();
        release_lock.recv_timeout(WAIT).unwrap();
    });
    ready.await.unwrap();
    let request = quiesce(&cancel, WAIT);
    tokio::pin!(request);
    assert!(futures::poll!(&mut request).is_pending());
    finish.send(()).unwrap();
    tokio::time::timeout(WAIT, dropping).await.unwrap().unwrap();
    let reader_early = tokio::time::timeout(Duration::from_millis(50), &mut request).await;
    release.send(()).unwrap();
    let cleanup_early = if reader_early.is_err() {
        tokio::time::timeout(Duration::from_millis(50), &mut request).await
    } else {
        reader_early
    };
    unlock.send(()).unwrap();
    cleanup.join().unwrap();
    assert!(
        cleanup_early.is_err(),
        "ACK preceded reader or registry cleanup"
    );
    assert_eq!(request.await, QuiesceAck::Quiesced);
    task.await.unwrap();
}

#[tokio::test]
async fn quiesce_rejects_missing_duplicate_and_uncertain_registration() {
    let cancel = Arc::new(AtomicBool::new(false));
    assert_eq!(quiesce(&cancel, WAIT).await, QuiesceAck::Busy);
    let original = Registration::new(cancel.clone());
    let request = quiesce(&cancel, WAIT);
    tokio::pin!(request);
    assert!(futures::poll!(&mut request).is_pending());
    assert_eq!(quiesce(&cancel, WAIT).await, QuiesceAck::Busy);
    let duplicate = Registration::new(cancel.clone());
    assert!(!duplicate.may_poll());
    assert!(!duplicate.needs_cleanup());
    original.finish(Outcome::Returned, false);
    assert_eq!(request.await, QuiesceAck::Busy);
    drop(duplicate);
    let cancel = Arc::new(AtomicBool::new(false));
    let original = Registration::new(cancel.clone());
    let duplicate = Registration::new(cancel.clone());
    assert_eq!(quiesce(&cancel, WAIT).await, QuiesceAck::Busy);
    drop((original, duplicate));
}

#[tokio::test]
async fn quiesce_timeout_does_not_drop_watcher_or_allow_repeat() {
    let cancel = Arc::new(AtomicBool::new(false));
    let (finish, resume) = oneshot::channel();
    let task = spawn(cancel.clone(), async {
        resume.await.unwrap();
    });
    assert_eq!(
        quiesce(&cancel, Duration::from_millis(10)).await,
        QuiesceAck::Busy
    );
    assert!(!task.is_finished());
    assert_eq!(quiesce(&cancel, WAIT).await, QuiesceAck::Busy);
    finish.send(()).unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn quiesce_success_prevents_same_cancel_reregistration_poll() {
    let cancel = Arc::new(AtomicBool::new(false));
    let (finish, resume) = oneshot::channel();
    let task = spawn(cancel.clone(), async {
        resume.await.unwrap();
    });
    let request = quiesce(&cancel, WAIT);
    tokio::pin!(request);
    assert!(futures::poll!(&mut request).is_pending());
    finish.send(()).unwrap();
    assert_eq!(request.await, QuiesceAck::Quiesced);
    task.await.unwrap();
    let polled = Arc::new(AtomicBool::new(false));
    let flag = polled.clone();
    spawn(cancel, async move {
        flag.store(true, std::sync::atomic::Ordering::Release);
    })
    .await
    .unwrap();
    assert!(!polled.load(std::sync::atomic::Ordering::Acquire));
}
