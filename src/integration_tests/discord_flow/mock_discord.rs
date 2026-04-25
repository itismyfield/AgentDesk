//! Mock Discord HTTP transport for the flow harness (#1073).
//!
//! The real outbound path speaks HTTP via [`HttpOutboundClient`]; the mock
//! below implements the same [`DiscordOutboundClient`] trait but records
//! every request in-memory and returns scripted responses. This lets the
//! harness assert "exactly one relay sent" without spinning up an HTTP
//! server, and lets the duplicate-relay scenario inject a retry without
//! touching a real Discord token.
//!
//! Scope is intentionally narrow:
//! - record the `(target_channel, content)` tuple per call
//! - allow the test to script a finite sequence of failures before the next
//!   success
//! - return either a length error (`MessageTooLong`) or a generic
//!   [`DispatchMessagePostErrorKind::Other`] failure
//!
//! The mock is cheap to clone (all state lives behind `Arc<Mutex<_>>`) so
//! scenarios can hold one reference for assertions while the outbound
//! pipeline holds another for the send path.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::server::routes::dispatches::discord_delivery::{
    DispatchMessagePostError, DispatchMessagePostErrorKind,
};
use crate::services::discord::outbound::DiscordOutboundClient;

/// Scripted failure entry. `(min_len, is_length_error)` — the next call whose
/// content reaches `min_len` characters consumes this slot and returns the
/// matching error. Set `min_len = 0` to fail unconditionally on the next
/// call.
#[derive(Clone, Copy, Debug)]
struct FailureSlot {
    min_len: usize,
    is_length_error: bool,
}

/// One recorded outbound call. Exposed through [`MockDiscord::recorded`] so
/// scenarios can count relays and match on content. Fields are public to
/// the integration-test crate so future scenarios can filter by
/// content; `#[allow(dead_code)]` silences the warning until they do.
#[allow(dead_code)]
#[derive(Clone, Debug)]
pub(crate) struct RecordedSend {
    pub(crate) target_channel: String,
    pub(crate) content: String,
}

/// In-memory double for the Discord HTTP outbound layer.
#[derive(Clone, Default)]
pub(crate) struct MockDiscord {
    inner: Arc<MockDiscordInner>,
}

#[derive(Default)]
struct MockDiscordInner {
    calls: Mutex<Vec<RecordedSend>>,
    failures: Mutex<Vec<FailureSlot>>,
    /// If set, every call fails with a generic error. Used by the scenario
    /// that wants to verify dedupe still short-circuits ahead of transport.
    always_fail: Mutex<bool>,
    call_count: AtomicUsize,
}

impl MockDiscord {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Enqueue a "next call fails with length error if content ≥ min_len".
    #[allow(dead_code)]
    pub(crate) fn push_length_failure(&self, min_len: usize) {
        self.inner.failures.lock().unwrap().push(FailureSlot {
            min_len,
            is_length_error: true,
        });
    }

    /// Enqueue a generic failure (non-length) for the next call whose
    /// content reaches `min_len` characters. Pass `0` to fail unconditionally.
    #[allow(dead_code)]
    pub(crate) fn push_generic_failure(&self, min_len: usize) {
        self.inner.failures.lock().unwrap().push(FailureSlot {
            min_len,
            is_length_error: false,
        });
    }

    #[allow(dead_code)]
    pub(crate) fn set_always_fail(&self, fail: bool) {
        *self.inner.always_fail.lock().unwrap() = fail;
    }

    #[allow(dead_code)]
    pub(crate) fn recorded(&self) -> Vec<RecordedSend> {
        self.inner.calls.lock().unwrap().clone()
    }

    pub(crate) fn call_count(&self) -> usize {
        self.inner.call_count.load(Ordering::SeqCst)
    }

    /// Count calls whose `target_channel` matches `channel_id`. Handy for the
    /// duplicate-relay scenario where only one target is under test.
    pub(crate) fn calls_to(&self, channel_id: &str) -> usize {
        self.inner
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.target_channel == channel_id)
            .count()
    }
}

impl DiscordOutboundClient for MockDiscord {
    async fn post_message(
        &self,
        target_channel: &str,
        content: &str,
    ) -> Result<String, DispatchMessagePostError> {
        let call_index = self.inner.call_count.fetch_add(1, Ordering::SeqCst);
        self.inner.calls.lock().unwrap().push(RecordedSend {
            target_channel: target_channel.to_string(),
            content: content.to_string(),
        });

        if *self.inner.always_fail.lock().unwrap() {
            return Err(DispatchMessagePostError::new(
                DispatchMessagePostErrorKind::Other,
                "mock discord: always_fail".to_string(),
            ));
        }

        let mut failures = self.inner.failures.lock().unwrap();
        if let Some(first) = failures.first().copied() {
            if content.chars().count() >= first.min_len {
                failures.remove(0);
                let kind = if first.is_length_error {
                    DispatchMessagePostErrorKind::MessageTooLong
                } else {
                    DispatchMessagePostErrorKind::Other
                };
                return Err(DispatchMessagePostError::new(
                    kind,
                    format!("mock discord: scripted failure #{call_index}"),
                ));
            }
        }

        Ok(format!("mock-msg-{call_index}"))
    }
}
