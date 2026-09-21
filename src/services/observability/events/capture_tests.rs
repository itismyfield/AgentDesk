use super::{EventLog, StructuredEvent, record_simple, test_capture::*};
use serde_json::json;
use std::future::{Future, pending, poll_fn};
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

fn emit(name: &str) {
    record_simple(name, None, None, json!({"identity": name}));
}
fn poll<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}

#[test]
fn sync_capture_restores_nested_and_panicking_scopes() {
    let (_, rows) = capture_sync(|| {
        emit("outer");
        let (_, inner) = capture_sync(|| emit("inner"));
        assert_eq!(inner[0].event_type, "inner");
        assert!(
            std::panic::catch_unwind(|| capture_sync(|| {
                emit("panicking");
                panic!("intentional scope unwind");
            }))
            .is_err()
        );
        EventLog::new(2).push(StructuredEvent::new("local", None, None, json!({})));
        assert_eq!(snapshot().len(), 1);
    });
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].event_type, "outer");
    emit("unscoped");
    assert!(std::panic::catch_unwind(snapshot).is_err());
    assert!(capture_sync(snapshot).1.is_empty());
}

#[tokio::test]
async fn async_capture_excludes_spawn_and_restores_after_cancellation() {
    let (_, rows) = capture_async(async {
        emit("parent");
        tokio::spawn(async {
            emit("foreign child");
        })
        .await
        .unwrap();
        let (_, child) = tokio::spawn(capture_async(async {
            emit("owned child");
        }))
        .await
        .unwrap();
        assert_eq!(child.len(), 1);
        assert_eq!(child[0].event_type, "owned child");
        let (_, nested) = capture_async(async {
            emit("nested");
        })
        .await;
        assert_eq!(nested[0].event_type, "nested");
        let mut cancelled = Box::pin(capture_async(async {
            emit("cancelled");
            pending::<()>().await;
        }));
        assert!(poll(cancelled.as_mut()).is_pending());
        drop(cancelled);
        let mut panicking = Box::pin(capture_async::<()>(async {
            emit("async panic");
            panic!("intentional async unwind");
        }));
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| poll(panicking.as_mut())))
                .is_err()
        );
        drop(panicking);
        assert_eq!(snapshot().len(), 1);
    })
    .await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].event_type, "parent");
    assert!(std::panic::catch_unwind(snapshot).is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scoped_future_moves_between_runtime_threads_without_leaking() {
    let (send_future, receive_future) = tokio::sync::oneshot::channel();
    let (release, held) = std::sync::mpsc::channel();
    let first = tokio::task::spawn_blocking(move || {
        let mut threads = Vec::new();
        let mut future = Box::pin(capture_async(poll_fn(move |_| {
            threads.push(std::thread::current().id());
            emit("moving");
            if threads.len() == 1 {
                Poll::Pending
            } else {
                Poll::Ready(threads.clone())
            }
        })));
        assert!(poll(future.as_mut()).is_pending());
        emit("foreign on same thread");
        send_future
            .send(future)
            .unwrap_or_else(|_| panic!("future receiver dropped"));
        held.recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
    });
    let mut future = receive_future.await.unwrap();
    let output = tokio::task::spawn_blocking(move || {
        let output = poll(future.as_mut());
        release.send(()).unwrap();
        output
    })
    .await
    .unwrap();
    first.await.unwrap();
    let Poll::Ready((threads, rows)) = output else {
        panic!("second poll must finish")
    };
    assert_ne!(threads[0], threads[1]);
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|row| row.event_type == "moving"));
}

#[test]
fn drain_commit_preserves_complete_recent_rows() {
    let log = EventLog::new(4);
    for name in ["first", "second"] {
        log.push(StructuredEvent::new(
            name,
            Some(42),
            Some("Codex"),
            json!({"id": name}),
        ));
    }
    let before = serde_json::to_value(log.recent(4)).unwrap();
    let (drained, watermark) = log.drain_unflushed();
    assert_eq!(serde_json::to_value(drained).unwrap(), before);
    assert_eq!(serde_json::to_value(log.recent(4)).unwrap(), before);
    log.commit_flushed(watermark);
    assert!(log.drain_unflushed().0.is_empty());
    assert_eq!(serde_json::to_value(log.recent(4)).unwrap(), before);
}
