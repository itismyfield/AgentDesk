//! A foreground turn's final launch instructions, joined with its observed native identity.
use std::cell::RefCell;
use std::sync::{Arc, Mutex};

use chrono::Utc;
use sqlx::PgPool;

use super::{CancelToken, ProviderKind};
use crate::db::provider_session_prompts::{self, SessionPrompt};

#[derive(Default)]
struct State {
    launch: Option<(String, chrono::DateTime<Utc>)>,
    session: Option<String>,
    success: bool,
    saved: bool,
}

pub(crate) struct Capture {
    pool: Option<PgPool>,
    provider: String,
    channel: String,
    turn: String,
    anchors: i64,
    state: Mutex<State>,
}

thread_local! {
    static ACTIVE: RefCell<Option<Arc<Capture>>> = const { RefCell::new(None) };
}

pub(crate) fn arm(
    token: &CancelToken,
    pool: Option<PgPool>,
    provider: &ProviderKind,
    channel: u64,
    turn: &str,
    anchors: usize,
) {
    if matches!(provider, ProviderKind::Claude | ProviderKind::Codex) {
        let _ = token.prompt_capture.set(Arc::new(Capture {
            pool,
            provider: provider.as_str().into(),
            channel: channel.to_string(),
            turn: turn.into(),
            anchors: i64::try_from(anchors).unwrap_or(i64::MAX),
            state: Mutex::new(State::default()),
        }));
    }
}

pub(crate) struct Scope {
    capture: Option<Arc<Capture>>,
    previous: Option<Arc<Capture>>,
}

impl Scope {
    pub(crate) fn enter(token: &CancelToken) -> Self {
        let capture = token.prompt_capture.get().cloned();
        let previous = ACTIVE.replace(capture.clone());
        Self { capture, previous }
    }

    pub(crate) fn success(&self) {
        if let Some(capture) = &self.capture {
            capture
                .state
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .success = true;
            capture.flush();
        }
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        ACTIVE.set(self.previous.take());
    }
}

pub(crate) fn record(prompt: Option<&str>, resume: bool) {
    ACTIVE.with_borrow(|active| {
        if let Some(capture) = active {
            capture
                .state
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .launch = (!resume).then(|| (prompt.unwrap_or("").to_owned(), Utc::now()));
        }
    });
}

pub(crate) fn claude_args(args: &[String]) {
    record(
        args.windows(2)
            .find(|p| p[0] == "--append-system-prompt")
            .map(|p| p[1].as_str()),
        args.iter().any(|arg| arg == "--resume"),
    );
}

pub(crate) fn codex_options(options: &crate::services::codex::CodexLaunchOptions) {
    record(
        options.developer_instructions.as_deref(),
        options.resume_session_id.is_some(),
    );
}

pub(crate) fn observe(token: &CancelToken, session: &str) {
    if let Some(capture) = token.prompt_capture.get() {
        capture
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .session = Some(session.into());
        capture.flush();
    }
}

pub(crate) fn observed_binding(token: &CancelToken) -> Option<(ProviderKind, String)> {
    let capture = token.prompt_capture.get()?;
    let session = capture
        .state
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .session
        .clone()?;
    Some((ProviderKind::from_str(&capture.provider)?, session))
}

impl Capture {
    fn take_ready(&self) -> Option<SessionPrompt> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if !state.success || state.saved {
            return None;
        }
        let (prompt, started_at) = state.launch.as_ref()?;
        let session = state.session.as_ref()?;
        let snapshot = SessionPrompt {
            provider: self.provider.clone(),
            channel_id: self.channel.clone(),
            provider_session_id: session.clone(),
            source_turn_id: self.turn.clone(),
            started_at: *started_at,
            system_prompt: prompt.clone(),
            anchor_count: if prompt.is_empty() { 0 } else { self.anchors },
        };
        state.saved = true;
        Some(snapshot)
    }

    fn flush(&self) {
        let Some(pool) = self.pool.clone() else {
            return;
        };
        let Some(snapshot) = self.take_ready() else {
            return;
        };
        tokio::spawn(async move {
            if let Err(error) = provider_session_prompts::save(&pool, &snapshot).await {
                tracing::warn!(%error, "session system prompt snapshot could not be saved");
            }
        });
    }
}

#[cfg(test)]
mod tests;
