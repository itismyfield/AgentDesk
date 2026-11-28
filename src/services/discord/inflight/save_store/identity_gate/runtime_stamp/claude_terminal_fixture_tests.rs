use super::*;
use crate::services::discord::{SharedData, mailbox_try_start_turn};
use crate::services::provider::CancelToken;
use crate::services::tui_prompt_dedupe as dedupe;
use serenity::all::{ChannelId, MessageId, UserId};
use std::sync::Arc;

pub(in crate::services::discord) struct Fixture {
    pub(in crate::services::discord) shared: Arc<crate::services::discord::SharedData>,
    pub(in crate::services::discord) actor: Arc<CancelToken>,
    pub(in crate::services::discord) local: InflightTurnState,
    pub(in crate::services::discord) baseline: InflightTurnState,
    pub(in crate::services::discord) expected: InflightTurnIdentity,
    pub(in crate::services::discord) transcript: PathBuf,
    pub(in crate::services::discord) tmux: String,
    pub(in crate::services::discord) generation: i64,
    pub(in crate::services::discord) file: (u64, u64),
    pub(in crate::services::discord) end: u64,
}

impl Fixture {
    pub(in crate::services::discord) async fn new(
        root: &Path,
        index: u64,
        provider: ProviderKind,
        shared: Arc<SharedData>,
    ) -> Self {
        let runtime = if provider == ProviderKind::Codex {
            RuntimeHandoffKind::CodexTui
        } else {
            RuntimeHandoffKind::ClaudeTui
        };
        let actor = Arc::new(CancelToken::from_persisted_turn_nonce(Some(
            "same-nonce".into(),
        )));
        let channel = ChannelId::new(583_350_000 + index);
        let tmux = format!("AgentDesk-claude-terminal-source-5833-{index}");
        let transcript = root.join(format!("claude-{index}.jsonl"));
        std::fs::write(&transcript, b"{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"answer\"}]}}\n{\"type\":\"result\",\"result\":\"answer\"}\n").unwrap();
        let transcript = std::fs::canonicalize(transcript).unwrap();
        let end = std::fs::metadata(&transcript).unwrap().len();
        std::fs::write(
            crate::services::tmux_common::session_temp_path(&tmux, "generation"),
            b"1",
        )
        .unwrap();
        let generation = tmux_generation_file_mtime_ns(&tmux);
        dedupe::register_tmux_runtime_binding(
            &tmux,
            dedupe::TuiRuntimeBinding {
                runtime_kind: runtime,
                output_path: transcript.display().to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: None,
                last_offset: 0,
                relay_last_offset: None,
            },
        );
        let mut local = InflightTurnState::new(
            provider,
            channel.get(),
            None,
            343_742_347_365_974_026,
            77_010,
            18,
            "source witness".into(),
            None,
            Some(tmux.clone()),
            Some(transcript.display().to_string()),
            None,
            0,
        );
        local.runtime_kind = Some(runtime);
        local.turn_nonce = actor.turn_nonce().map(str::to_owned);
        local.turn_start_offset = Some(0);
        assert!(
            mailbox_try_start_turn(
                &shared,
                channel,
                actor.clone(),
                UserId::new(local.request_owner_user_id),
                MessageId::new(local.user_msg_id)
            )
            .await
        );
        let runtime_root = inflight_runtime_root().unwrap();
        save_inflight_state_in_root(&runtime_root, &local).unwrap();
        let expected = InflightTurnIdentity::from_state(&local);
        Self {
            shared,
            actor,
            baseline: local.clone(),
            expected,
            local,
            file: file_identity(&transcript).unwrap(),
            transcript,
            tmux,
            generation,
            end,
        }
    }

    pub(in crate::services::discord) fn frame(&self) -> StreamMessage {
        if self.local.provider_kind() == Some(ProviderKind::Codex) {
            return StreamMessage::CodexTuiTerminalDone {
                result: "answer".into(),
                session_id: None,
                rollout_path: self.transcript.display().to_string(),
                tmux_session_name: self.tmux.clone(),
                turn_nonce: "same-nonce".into(),
                source_start: 0,
                complete_record_end: self.end,
                captured_source: Some(crate::services::agent_protocol::CapturedTuiTerminalSource {
                    generation_mtime_ns: self.generation,
                    source_file_dev: self.file.0,
                    source_file_ino: self.file.1,
                    actor: Arc::downgrade(&self.actor),
                }),
                kind: crate::services::agent_protocol::NativeTerminalKind::Completed,
            };
        }
        StreamMessage::ClaudeTuiTerminalDone {
            result: "answer".into(),
            session_id: None,
            transcript_path: self.transcript.display().to_string(),
            tmux_session_name: self.tmux.clone(),
            turn_nonce: "same-nonce".into(),
            source_start: 0,
            complete_record_end: self.end,
            generation_mtime_ns: self.generation,
            source_file_dev: self.file.0,
            source_file_ino: self.file.1,
            actor: Arc::downgrade(&self.actor),
            kind: crate::services::agent_protocol::NativeTerminalKind::Completed,
        }
    }

    pub(in crate::services::discord) async fn admit(
        &mut self,
        frame: StreamMessage,
    ) -> Result<(StreamMessage, Option<TuiTerminalRange>, bool), GuardedSaveOutcome> {
        self.local
            .admit_tui_terminal_frame(
                &mut self.baseline,
                &self.expected,
                true,
                (&self.shared, &self.actor),
                "answer",
                frame,
            )
            .await
            .map(|(message, range, terminal, _)| (message, range, terminal))
    }

    pub(in crate::services::discord) fn durable(&self) -> Vec<u8> {
        std::fs::read(inflight_state_path(
            &inflight_runtime_root().unwrap(),
            &self.local.provider_kind().unwrap(),
            self.local.channel_id,
        ))
        .unwrap()
    }
}
