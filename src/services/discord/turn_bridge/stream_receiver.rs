//! Async adapter and wait policy for the turn bridge's blocking stream receiver.

use super::*;

pub(in crate::services::discord) struct StreamMessageReceiverAdapter {
    rx: tokio::sync::mpsc::UnboundedReceiver<StreamMessage>,
    stop: Arc<std::sync::atomic::AtomicBool>,
}

impl StreamMessageReceiverAdapter {
    pub(super) async fn recv(&mut self) -> Option<StreamMessage> {
        self.rx.recv().await
    }

    pub(super) fn try_recv(
        &mut self,
    ) -> Result<StreamMessage, tokio::sync::mpsc::error::TryRecvError> {
        self.rx.try_recv()
    }
}

impl Drop for StreamMessageReceiverAdapter {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
    }
}

pub(in crate::services::discord) fn spawn_stream_message_receiver_adapter(
    rx: mpsc::Receiver<StreamMessage>,
) -> StreamMessageReceiverAdapter {
    let (tx, async_rx) = tokio::sync::mpsc::unbounded_channel();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_worker = stop.clone();
    tokio::task::spawn_blocking(move || {
        while !stop_worker.load(std::sync::atomic::Ordering::Acquire) {
            match rx.recv_timeout(std::time::Duration::from_millis(10)) {
                Ok(message) => {
                    if stop_worker.load(std::sync::atomic::Ordering::Acquire)
                        || tx.send(message).is_err()
                    {
                        break;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    });
    StreamMessageReceiverAdapter { rx: async_rx, stop }
}

pub(in crate::services::discord) fn turn_bridge_stream_wait_duration(
    done: bool,
    terminal_control_drain_until: Option<std::time::Instant>,
    now: std::time::Instant,
) -> std::time::Duration {
    if done {
        return terminal_control_drain_until
            .map(|deadline| deadline.saturating_duration_since(now))
            .unwrap_or_else(|| std::time::Duration::from_millis(0));
    }
    std::time::Duration::from_secs(1)
}

#[cfg(test)]
mod ready_drain_unit_tests {
    use super::*;

    #[test]
    fn done_wait_uses_remaining_drain_window_as_safety_wake() {
        let now = std::time::Instant::now();
        assert_eq!(
            turn_bridge_stream_wait_duration(
                true,
                Some(now + std::time::Duration::from_millis(123)),
                now,
            ),
            std::time::Duration::from_millis(123)
        );
        assert_eq!(
            turn_bridge_stream_wait_duration(true, None, now),
            std::time::Duration::from_millis(0)
        );
        assert_eq!(
            turn_bridge_stream_wait_duration(false, None, now),
            std::time::Duration::from_secs(1)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stream_receiver_adapter_wakes_on_ready_frame() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut async_rx = spawn_stream_message_receiver_adapter(rx);
        tx.send(StreamMessage::TmuxReady {
            output_path: "/tmp/out.jsonl".to_string(),
            input_fifo_path: "/tmp/in.fifo".to_string(),
            tmux_session_name: "adk-test".to_string(),
            last_offset: 12,
        })
        .expect("send ready frame");

        let received = tokio::time::timeout(std::time::Duration::from_millis(50), async_rx.recv())
            .await
            .expect("ready frame should wake without a poll tick")
            .expect("adapter should forward ready frame");

        assert!(matches!(received, StreamMessage::TmuxReady { .. }));
    }
}
