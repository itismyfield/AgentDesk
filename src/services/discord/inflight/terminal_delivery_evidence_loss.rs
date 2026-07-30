//! Operator-visible routing for terminal-delivery evidence loss (#5025).

use serenity::all::{ChannelId, MessageId};

use super::{GuardedSaveOutcome, WatcherTerminalCommitOutcome};
use crate::services::provider::ProviderKind;

pub(in crate::services::discord) struct WatcherEvidenceLossContext<'a> {
    pub(in crate::services::discord) provider: &'a ProviderKind,
    pub(in crate::services::discord) channel_id: ChannelId,
    pub(in crate::services::discord) tmux_session_name: &'a str,
    pub(in crate::services::discord) expected_user_msg_id: u64,
    pub(in crate::services::discord) last_offset: u64,
    pub(in crate::services::discord) turn_data_start_offset: u64,
}

pub(in crate::services::discord) fn warn_for_watcher_terminal_commit_outcome(
    outcome: WatcherTerminalCommitOutcome,
    ctx: WatcherEvidenceLossContext<'_>,
) -> bool {
    match outcome {
        WatcherTerminalCommitOutcome::Committed => true,
        WatcherTerminalCommitOutcome::Skipped => {
            tracing::warn!(
                provider = %ctx.provider.as_str(),
                channel_id = ctx.channel_id.get(),
                tmux_session = %ctx.tmux_session_name,
                expected_user_msg_id = ctx.expected_user_msg_id,
                last_offset = ctx.last_offset,
                turn_data_start_offset = ctx.turn_data_start_offset,
                "watcher relayed a terminal answer but the inflight identity guard refused the commit; this turn's delivery evidence is lost (row will read as undelivered)"
            );
            false
        }
        WatcherTerminalCommitOutcome::IoError => {
            tracing::warn!(
                provider = %ctx.provider.as_str(),
                channel_id = ctx.channel_id.get(),
                tmux_session = %ctx.tmux_session_name,
                "watcher failed to mirror committed terminal delivery into inflight state"
            );
            false
        }
    }
}

pub(in crate::services::discord) struct BridgeEvidenceLossContext<'a> {
    pub(in crate::services::discord) provider: &'a ProviderKind,
    pub(in crate::services::discord) channel_id: ChannelId,
    pub(in crate::services::discord) current_msg_id: MessageId,
    pub(in crate::services::discord) response_sent_offset: usize,
}

pub(in crate::services::discord) fn warn_for_bridge_terminal_mirror_outcome(
    outcome: GuardedSaveOutcome,
    ctx: BridgeEvidenceLossContext<'_>,
) {
    match outcome {
        GuardedSaveOutcome::Saved | GuardedSaveOutcome::Missing => {}
        GuardedSaveOutcome::IdentityMismatch => {
            tracing::warn!(
                provider = %ctx.provider.as_str(),
                channel_id = ctx.channel_id.get(),
                current_msg_id = ctx.current_msg_id.get(),
                response_sent_offset = ctx.response_sent_offset,
                "turn bridge delivered the terminal answer but could not mirror terminal_delivery_committed: the inflight row is owned by a different turn identity, so this turn's delivery evidence is lost (row will read as undelivered)"
            );
        }
        GuardedSaveOutcome::IoError => {
            tracing::warn!(
                provider = %ctx.provider.as_str(),
                channel_id = ctx.channel_id.get(),
                "turn bridge failed to mirror committed terminal delivery before cleanup"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{self, Write},
        sync::{Arc, Mutex},
    };

    use super::*;
    use tracing_subscriber::fmt::MakeWriter;

    #[derive(Clone)]
    struct CapturingWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for CapturingWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for CapturingWriter {
        type Writer = CapturingWriter;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    fn capture_warn(run: impl FnOnce()) -> String {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .without_time()
            .with_writer(CapturingWriter(buffer.clone()))
            .finish();
        tracing::subscriber::with_default(subscriber, run);
        String::from_utf8(buffer.lock().unwrap().clone()).unwrap()
    }

    #[test]
    fn skipped_watcher_outcome_emits_lost_evidence_warn() {
        let logs = capture_warn(|| {
            assert!(!warn_for_watcher_terminal_commit_outcome(
                WatcherTerminalCommitOutcome::Skipped,
                WatcherEvidenceLossContext {
                    provider: &ProviderKind::Claude,
                    channel_id: ChannelId::new(5025),
                    tmux_session_name: "AgentDesk-claude-5025",
                    expected_user_msg_id: 77,
                    last_offset: 128,
                    turn_data_start_offset: 64,
                },
            ));
        });

        assert!(
            logs.contains("watcher relayed a terminal answer but the inflight identity guard refused the commit"),
            "logs={logs}"
        );
    }

    #[test]
    fn identity_mismatch_bridge_outcome_emits_lost_evidence_warn() {
        let logs = capture_warn(|| {
            warn_for_bridge_terminal_mirror_outcome(
                GuardedSaveOutcome::IdentityMismatch,
                BridgeEvidenceLossContext {
                    provider: &ProviderKind::Claude,
                    channel_id: ChannelId::new(5025),
                    current_msg_id: MessageId::new(5026),
                    response_sent_offset: 128,
                },
            );
        });

        assert!(
            logs.contains("turn bridge delivered the terminal answer but could not mirror terminal_delivery_committed"),
            "logs={logs}"
        );
    }
}
