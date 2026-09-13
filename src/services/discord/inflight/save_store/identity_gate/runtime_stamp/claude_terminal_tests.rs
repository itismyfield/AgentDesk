use super::*;
use crate::services::discord::{
    mailbox_recovery_kickoff, mailbox_try_start_turn, make_shared_data_for_tests,
};
use crate::services::provider::CancelToken;
use crate::services::tui_prompt_dedupe as dedupe;
use serenity::all::{ChannelId, MessageId, UserId};
use std::sync::Arc;

struct Fixture {
    shared: Arc<crate::services::discord::SharedData>,
    actor: Arc<CancelToken>,
    local: InflightTurnState,
    baseline: InflightTurnState,
    expected: InflightTurnIdentity,
    transcript: PathBuf,
    tmux: String,
    generation: i64,
    file: (u64, u64),
    end: u64,
}

impl Fixture {
    async fn new(root: &Path, index: u64) -> Self {
        let shared = make_shared_data_for_tests();
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
                runtime_kind: RuntimeHandoffKind::ClaudeTui,
                output_path: transcript.display().to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: None,
                last_offset: 0,
                relay_last_offset: None,
            },
        );
        let mut local = InflightTurnState::new(
            ProviderKind::Claude,
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
        local.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
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

    fn frame(&self) -> StreamMessage {
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
        }
    }

    async fn admit(
        &mut self,
        frame: StreamMessage,
    ) -> Result<(StreamMessage, Option<TuiTerminalRange>, bool), GuardedSaveOutcome> {
        self.local
            .admit_tui_terminal_frame(
                &mut self.baseline,
                &self.expected,
                true,
                &self.shared,
                &self.actor,
                "answer",
                frame,
            )
            .await
    }

    fn durable(&self) -> Vec<u8> {
        std::fs::read(inflight_state_path(
            &inflight_runtime_root().unwrap(),
            &ProviderKind::Claude,
            self.local.channel_id,
        ))
        .unwrap()
    }
}

#[test]
fn claude_terminal_range_admits_actual_file_and_retains_receipt_after_cursor_progress() {
    let temp = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
    let _dedupe = dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let mut fixture = Fixture::new(temp.path(), 1).await;
            let mut frame = fixture.frame();
            // A parsed terminal may omit result while the same reader streamed the body.
            if let StreamMessage::ClaudeTuiTerminalDone { result, .. } = &mut frame {
                result.clear();
            }
            let (done, range, terminal) = fixture.admit(frame).await.unwrap();
            assert!(terminal);
            assert!(matches!(done, StreamMessage::Done { result, .. } if result == "answer"));
            let range = range.unwrap();
            assert_eq!(range.source.provider, "claude");
            assert_eq!(range.source.range, (0, fixture.end));
            assert_eq!(range.source_file_identity, Some(fixture.file));
            assert!(range.revalidated_source(&fixture.local).unwrap().is_some());
            crate::services::tmux_common::with_tmux_source_authority(&fixture.tmux, |authority| {
                assert!(range.source_authority_is_live(authority));
                assert!(range.source_receipt_is_live(authority));
            });
            let mut binding = dedupe::runtime_binding_for_tmux_session(&fixture.tmux).unwrap();
            binding.last_offset = fixture.end + 10;
            dedupe::register_tmux_runtime_binding(&fixture.tmux, binding);
            crate::services::tmux_common::with_tmux_source_authority(&fixture.tmux, |authority| {
                assert!(!range.source_authority_is_live(authority));
                assert!(range.source_receipt_is_live(authority));
            });
            // Reusing the pathname in the same generation cannot lend its identity
            // to the old file descriptor, either for publication or receipt lookup.
            std::fs::rename(
                &fixture.transcript,
                fixture.transcript.with_extension("old"),
            )
            .unwrap();
            std::fs::write(&fixture.transcript, vec![b'x'; fixture.end as usize]).unwrap();
            assert!(range.receipt_source_path().is_none());
            assert!(range.revalidated_source(&fixture.local).unwrap().is_none());
        });
}

#[test]
fn claude_terminal_range_rejects_unproven_source_and_same_nonce_successor_without_row_mutation() {
    let temp = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
    let _dedupe = dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            for (index, defect) in [
                "different-fd",
                "rotated-file",
                "generation",
                "empty-range",
                "reversed-range",
                "beyond-eof",
                "foreign-actor",
                "successor",
            ]
            .into_iter()
            .enumerate()
            {
                let mut fixture = Fixture::new(temp.path(), index as u64 + 10).await;
                let mut frame = fixture.frame();
                let mut foreign_actor = None;
                if let StreamMessage::ClaudeTuiTerminalDone {
                    source_file_ino,
                    generation_mtime_ns,
                    source_start,
                    complete_record_end,
                    actor,
                    ..
                } = &mut frame
                {
                    match defect {
                        "different-fd" => *source_file_ino = source_file_ino.wrapping_add(1),
                        "rotated-file" => {
                            std::fs::rename(
                                &fixture.transcript,
                                fixture.transcript.with_extension("old"),
                            )
                            .unwrap();
                            std::fs::write(&fixture.transcript, vec![b'x'; fixture.end as usize])
                                .unwrap();
                        }
                        "generation" => *generation_mtime_ns += 1,
                        "empty-range" => *complete_record_end = *source_start,
                        "reversed-range" => *source_start = *complete_record_end + 1,
                        "beyond-eof" => *complete_record_end += 1,
                        "foreign-actor" | "successor" => {
                            let successor = Arc::new(CancelToken::from_persisted_turn_nonce(Some(
                                "same-nonce".into(),
                            )));
                            if defect == "foreign-actor" {
                                *actor = Arc::downgrade(&successor);
                            } else {
                                mailbox_recovery_kickoff(
                                    &fixture.shared,
                                    ChannelId::new(fixture.local.channel_id),
                                    successor.clone(),
                                    UserId::new(fixture.local.request_owner_user_id),
                                    Some(MessageId::new(fixture.local.user_msg_id)),
                                )
                                .await;
                            }
                            foreign_actor = Some(successor);
                        }
                        _ => unreachable!(),
                    }
                }
                let before = fixture.durable();
                let local_before = serde_json::to_value(&fixture.local).unwrap();
                assert!(
                    fixture.admit(frame).await.is_err(),
                    "{defect} must preserve the obligation"
                );
                assert_eq!(fixture.durable(), before, "{defect} cannot write the row");
                assert_eq!(serde_json::to_value(&fixture.local).unwrap(), local_before);
                if defect == "successor" {
                    let current = crate::services::discord::mailbox_snapshot(
                        &fixture.shared,
                        ChannelId::new(fixture.local.channel_id),
                    )
                    .await;
                    assert!(
                        current
                            .cancel_token
                            .as_ref()
                            .is_some_and(|current| Arc::ptr_eq(
                                current,
                                foreign_actor.as_ref().unwrap()
                            ))
                    );
                }
            }
        });
}
