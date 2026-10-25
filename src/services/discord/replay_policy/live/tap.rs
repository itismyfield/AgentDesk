//! Protected readers publish activity before delivery and close exactly once.

use super::{Activity, LiveAttempt, Seen, lock, seal};
use crate::services::agent_protocol::StreamMessage;
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::provider_hosting::ProviderSessionDriver;
use std::sync::Arc;

#[path = "stderr.rs"]
mod stderr;
use stderr::StderrRead;

pub(crate) struct RawTap(Option<Arc<LiveAttempt>>);

impl RawTap {
    pub(crate) fn for_token(token: Option<&CancelToken>) -> Self {
        Self::claim(token, ProviderKind::Claude, None)
    }

    pub(crate) fn for_codex(token: Option<&CancelToken>) -> Self {
        Self::claim(token, ProviderKind::Codex, None)
    }

    pub(crate) fn for_file(token: Option<&CancelToken>) -> Self {
        Self::claim(
            token,
            ProviderKind::Claude,
            Some(crate::services::agent_protocol::RuntimeHandoffKind::ProcessBackend),
        )
    }

    fn claim(
        token: Option<&CancelToken>,
        provider: ProviderKind,
        runtime: Option<crate::services::agent_protocol::RuntimeHandoffKind>,
    ) -> Self {
        let live = token.and_then(CancelToken::replay_attempt).cloned();
        if let Some(live) = &live {
            let supported =
                seal::can_activate(provider, ProviderSessionDriver::LegacyPrompt, runtime);
            let mut observation = lock(&live.observation);
            if !observation.seal.claim(supported) {
                observation.append(Seen::Broken);
                live.recorded.notify_waiters();
                return Self(None);
            }
        }
        Self(live)
    }

    pub(crate) fn active(&self) -> bool {
        self.0.is_some()
    }

    pub(crate) fn see(&self, seen: Seen) {
        if let Some(live) = &self.0 {
            live.record_fact(seen);
        }
    }

    fn stale(&self, message: &str, stderr: &str) {
        if let Some(live) = &self.0
            && crate::services::discord::turn_bridge::is_stale_resume_error(message, stderr)
        {
            live.mark_stale();
        }
    }

    pub(crate) fn record_line(&self, record_type: &str, message: Option<&StreamMessage>) {
        if !self.active() {
            return;
        }
        if let Some(StreamMessage::Error {
            message, stderr, ..
        }) = message
        {
            self.stale(message, stderr);
        }
        if let Some(seen) = Seen::of_record(record_type, message) {
            self.see(seen);
        }
    }

    pub(crate) fn file_activity(&self) {
        if !self.active() {
            return;
        }
        self.see(Seen::Activity(Activity::Unclassified));
    }

    pub(crate) fn codex_line(&self, line: &str) {
        if !self.active() || line.trim().is_empty() {
            return;
        }
        match serde_json::from_str::<serde_json::Value>(line) {
            Ok(value)
                if matches!(
                    value["type"].as_str(),
                    Some("thread.started" | "turn.started" | "turn.completed" | "error")
                ) => {}
            Ok(_) => self.see(Seen::Activity(Activity::Unclassified)),
            Err(_) => self.see(Seen::Broken),
        }
    }

    pub(crate) fn start_stderr(&self, child: &mut std::process::Child) -> Option<StderrRead> {
        if !self.active() {
            return None;
        }
        child.stderr.take().map(StderrRead::start)
    }

    pub(crate) fn collect_stderr(&self, pending: Option<StderrRead>) -> (String, Seen) {
        match pending.and_then(|reader| reader.collect(STDERR_SETTLE).ok()) {
            Some(text) => (text, Seen::StderrDrained),
            None => (String::new(), Seen::Broken),
        }
    }

    pub(crate) fn read_stderr(&self, stderr: Option<std::process::ChildStderr>) -> (String, Seen) {
        if self.active() {
            self.collect_stderr(stderr.map(StderrRead::start))
        } else {
            match stderr.and_then(|stderr| std::io::read_to_string(stderr).ok()) {
                Some(text) => (text, Seen::StderrDrained),
                None => (String::new(), Seen::Broken),
            }
        }
    }

    pub(crate) fn error(&self, message: &str, stderr: &str) {
        self.stale(message, stderr);
    }

    pub(crate) fn finish_error(self, delivered: bool, drained: Seen, message: &str, stderr: &str) {
        self.stale(message, stderr);
        self.finish(delivered, drained);
    }

    pub(crate) fn finish(mut self, delivered: bool, drained: Seen) {
        if !delivered {
            self.see(Seen::Broken);
        }
        self.see(drained);
        if let Some(live) = self.0.take() {
            lock(&live.observation).seal.closed = true;
            live.recorded.notify_waiters();
        }
    }
}

impl Drop for RawTap {
    fn drop(&mut self) {
        if let Some(live) = &self.0 {
            live.record_fact(Seen::Broken);
            lock(&live.observation).seal.closed = true;
            live.recorded.notify_waiters();
        }
    }
}

const STDERR_SETTLE: std::time::Duration = std::time::Duration::from_secs(5);
