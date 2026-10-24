//! Preserve-stop admission: deliver once, keep the reader alive, then publish after native exit.

use super::claude_stop_delivery::{ClaudeStopTurnIdentity, interrupt_claude_native_turn};
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::discord::SharedData;
use crate::services::platform::tmux::{SessionPresence, session_presence};
use crate::services::provider::cancel_token_claude_interrupt::StopCancel;
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::turn_orchestrator::{CancelActiveTurnResult, ChannelMailboxHandle};
use poise::serenity_prelude::ChannelId;
use std::sync::{Arc, atomic::Ordering};

pub(in crate::services::discord) const UNCONFIRMED: &str =
    "중단 미확인: 중단 키 전달을 확인하지 못했습니다. 현재 턴과 실행 상태를 유지합니다.";
const SENT: &str = "중단 요청을 전달했어요. 같은 작업의 종료 기록을 기다려요.";

pub(in crate::services::discord) struct NativeStop {
    mailbox: ChannelMailboxHandle,
    token: Arc<CancelToken>,
    channel: ChannelId,
    session: String,
    reason: String,
    terminal: Arc<dyn Fn() -> bool + Send + Sync>,
    sent: bool,
    finished: bool,
    notify: bool,
}

pub(in crate::services::discord) fn protected(provider: &ProviderKind, session: &str) -> bool {
    if *provider != ProviderKind::Claude {
        return false;
    }
    let binding = crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(session);
    binding
        .map(|binding| binding.runtime_kind)
        .or_else(|| crate::services::tmux_common::resolve_tmux_runtime_kind_marker(session))
        == Some(RuntimeHandoffKind::ClaudeTui)
}

pub(in crate::services::discord) fn empty() -> CancelActiveTurnResult {
    CancelActiveTurnResult {
        token: None,
        already_stopping: false,
    }
}

fn confirmed_missing_session(session: &str) -> bool {
    match session_presence(session) {
        SessionPresence::Missing => true,
        SessionPresence::ProbeFailed => super::claude_stop_pane::mutant("probe_is_absent"),
        SessionPresence::Present => false,
    }
}

fn recovery_admissible(
    provider: &ProviderKind,
    channel: ChannelId,
    token: &Arc<CancelToken>,
    session: &str,
    completed: bool,
) -> Option<bool> {
    if token.tmux_session_name().as_deref() != Some(session) {
        return Some(true);
    }
    let live_owner =
        crate::services::discord::live_bridge::retain_original(provider, channel.get(), token)
            .is_some()
            && !super::claude_stop_pane::mutant("ignore_live_owner");
    match crate::services::discord::inflight::load_inflight_state_read_only_result(
        provider,
        channel.get(),
    ) {
        Ok(Some(row)) => {
            if row
                .tmux_session_name
                .as_deref()
                .is_some_and(|name| name != session)
                || row
                    .turn_nonce
                    .as_deref()
                    .zip(token.turn_nonce())
                    .is_some_and(|(row, token)| row != token)
            {
                return Some(true);
            }
            // Prelaunch rows have no transcript yet; a matching nonce still owns the row.
            if live_owner
                || token.turn_nonce().is_some() && row.turn_nonce.as_deref() == token.turn_nonce()
            {
                Some(false)
            } else {
                None
            }
        }
        Ok(None) => Some(completed && !live_owner),
        Err(_) => None,
    }
}

impl NativeStop {
    pub(in crate::services::discord) async fn begin(
        shared: &SharedData,
        provider: &ProviderKind,
        channel: ChannelId,
        token: &Arc<CancelToken>,
        session: &str,
        reason: &str,
    ) -> Option<Self> {
        if !protected(provider, session) || token.herdr_interrupt_state().is_some() {
            return None;
        }
        let mailbox = shared.mailbox_peek(channel)?;
        let mut stop = Self {
            mailbox,
            token: token.clone(),
            channel,
            session: session.into(),
            reason: reason.into(),
            terminal: Arc::new(|| false),
            sent: false,
            finished: false,
            notify: false,
        };
        if token.cancelled.load(Ordering::Acquire) {
            return None;
        }
        if !stop.current().await {
            stop.finished = true;
            return Some(stop);
        }
        if super::claude_stop_pane::mutant("native_always_none") {
            return None;
        }
        let completed = token.native_stop.is_completed();
        let recovery = recovery_admissible(provider, channel, token, session, completed);
        if !super::claude_stop_pane::mutant("protect_unowned")
            && (recovery == Some(true)
                || (token.native_stop.is_completed()
                    && super::claude_stop_pane::mutant("recovery_without_owner_gate")))
        {
            let weak = Arc::downgrade(token);
            let expected_binding = token.tmux_session_name();
            let expected_source =
                crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(session)
                    .map(|binding| binding.output_path);
            let (provider, session) = (provider.clone(), session.to_owned());
            let witness = Arc::new(move || {
                weak.upgrade().is_some_and(|token| {
                    !token.is_completion_cleanup()
                        && token.tmux_session_name() == expected_binding
                        && crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(
                            &session,
                        )
                        .map(|binding| binding.output_path)
                            == expected_source
                        && (recovery_admissible(&provider, channel, &token, &session, completed)
                            == Some(true)
                            || super::claude_stop_pane::mutant("recovery_without_owner_gate"))
                })
            });
            if token.native_stop.handoff_recovery(witness) {
                return None;
            }
        }
        if token.native_stop.is_completed() {
            stop.finished = true;
            return Some(stop);
        }
        if recovery.is_none() {
            stop.notify = token.native_stop.claim_notice();
            return Some(stop);
        }
        if !crate::services::discord::host_liveness::local_tmux(session, None) {
            return None;
        }
        if confirmed_missing_session(session) {
            let expected_binding = token.tmux_session_name();
            let weak = Arc::downgrade(token);
            let absent_session = session.to_owned();
            let absent = Arc::new(move || {
                crate::services::discord::host_liveness::local_tmux(&absent_session, None)
                    && confirmed_missing_session(&absent_session)
                    && weak
                        .upgrade()
                        .is_some_and(|token| token.tmux_session_name() == expected_binding)
            });
            if token.native_stop.handoff_idle(absent) {
                return None;
            }
            stop.finished = true;
            return Some(stop);
        }
        let binding = crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(session);
        let identity = binding
            .as_ref()
            .and_then(|binding| ClaudeStopTurnIdentity::capture(&binding.output_path));
        if let (Some(binding), Some(identity)) = (binding, identity) {
            let key = format!(
                "{}:{session}:{identity:?}",
                token.claude_interrupt_generation()
            );
            let path = binding.output_path;
            let expected_session = session.to_string();
            let weak = Arc::downgrade(token);
            let expected_identity = identity.clone();
            let terminal = Arc::new(move || {
                identity.still_current()
                    && crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(
                        &expected_session,
                    )
                    .is_some_and(|binding| {
                        binding.runtime_kind == RuntimeHandoffKind::ClaudeTui
                            && binding.output_path == path
                    })
                    && weak.upgrade().is_some_and(|token| {
                        token
                            .lock_current_claude_interrupt_session(&expected_session)
                            .is_some()
                    })
                    && crate::services::claude_tui::transcript_tail::observe_transcript_turn_state(
                        std::path::Path::new(&path),
                    ) == crate::services::tui_turn_state::TuiTurnState::Idle
                    && identity.still_current()
            });
            stop.terminal = terminal.clone();
            if terminal() {
                if !super::claude_stop_pane::mutant("idle_short_circuit")
                    && token.native_stop.handoff_idle(terminal.clone())
                {
                    return None;
                }
                stop.finished = true;
            } else if token.tmux_session_name().as_deref() == Some(session)
                && let Some(attempt) = token.native_stop.attempt(key, terminal)
            {
                if attempt.start() {
                    let (token, session, reason, attempt, identity) = (
                        token.clone(),
                        session.to_string(),
                        reason.to_string(),
                        attempt.clone(),
                        expected_identity,
                    );
                    tokio::spawn(async move {
                        let _completion = Completion(attempt.clone());
                        let sent = interrupt_claude_native_turn(&token, session, &reason, identity)
                            .await
                            .sent_keys;
                        attempt.complete(sent);
                    });
                }
                stop.sent = attempt.wait().await;
            }
        }
        if !stop.current().await || token.native_stop.is_completed() {
            stop.finished = true;
        }
        if !stop.sent && !stop.finished {
            stop.notify = token.native_stop.claim_notice();
        }
        if !stop.sent && super::claude_stop_pane::mutant("unconfirmed_cancel") {
            return None;
        }
        Some(stop)
    }

    async fn current(&self) -> bool {
        self.mailbox
            .cancel_token()
            .await
            .ok()
            .flatten()
            .is_some_and(|token| Arc::ptr_eq(&token, &self.token))
            && !self.token.is_completion_cleanup()
    }

    pub(in crate::services::discord) fn observed_text(sent: bool) -> &'static str {
        if sent { SENT } else { UNCONFIRMED }
    }

    pub(in crate::services::discord) fn text(&self) -> &'static str {
        if self.finished {
            "중단 대상 작업이 이미 끝났거나 바뀌었어요."
        } else if self.sent {
            SENT
        } else if self.notify {
            UNCONFIRMED
        } else {
            "같은 중단 요청의 결과를 이미 안내했어요."
        }
    }

    pub(in crate::services::discord) fn lifecycle_path(&self) -> &'static str {
        if self.finished {
            "native-stop-completed"
        } else if self.sent {
            "native-stop-pending"
        } else {
            "native-stop-unconfirmed"
        }
    }

    pub(in crate::services::discord) fn should_reply(&self) -> bool {
        self.sent || self.finished || self.notify
    }

    pub(in crate::services::discord) fn wire(&self) -> serde_json::Value {
        serde_json::json!({"outcome": if self.finished { "completed" } else if self.sent { "native_pending" } else { "unconfirmed" },
            "effect_started": self.sent, "notice": self.notify})
    }

    pub(in crate::services::discord) async fn notice(&self, shared: &SharedData) {
        if !self.notify {
            return;
        }
        if let Some(http) = shared.serenity_http_or_token_fallback() {
            if let Err(error) =
                crate::services::discord::outbound::confirmation::send_terminal_notice_http_once(
                    &http,
                    shared,
                    self.channel,
                    UNCONFIRMED,
                )
                .await
            {
                tracing::warn!(channel_id = self.channel.get(), %error, "native stop notice failed");
            }
        } else {
            tracing::warn!(
                channel_id = self.channel.get(),
                "native stop notice has no Discord transport"
            );
        }
    }

    pub(in crate::services::discord) async fn finish(&self) -> CancelActiveTurnResult {
        if !self.sent || self.finished {
            return empty();
        }
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if !self.current().await {
                return empty();
            }
            if (self.terminal)() || super::claude_stop_pane::mutant("proof_is_terminal") {
                let result = self
                    .mailbox
                    .cancel_active_turn_if_current_unless_herdr(
                        self.token.clone(),
                        self.reason.clone(),
                    )
                    .await;
                if let StopCancel::Published(token) = result {
                    crate::services::discord::record_turn_stop_tombstone(
                        self.channel,
                        Some(&self.session),
                        &self.reason,
                    )
                    .await;
                    return CancelActiveTurnResult {
                        token: Some(token),
                        already_stopping: false,
                    };
                }
                return empty();
            }
            if tokio::time::Instant::now() >= deadline {
                return empty();
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }
}

struct Completion(Arc<crate::services::provider::native_stop::Attempt>);
impl Drop for Completion {
    fn drop(&mut self) {
        self.0.complete(false);
    }
}
