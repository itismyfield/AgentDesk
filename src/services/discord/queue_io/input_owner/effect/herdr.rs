//! A busy Claude pane on Herdr: health's gated busy transaction under the owner scope, held to
//! the physical pane its execution runs on.
use std::path::PathBuf;

use super::{Completion, Leaf};
use crate::services::claude_tui::{busy_inject, owner_input};
use crate::services::discord::health::{BusyInput, InjectAttempt, herdr_busy};
use crate::services::session_host::HerdrTarget;

pub(crate) struct BusyClaudeHerdr {
    pub(crate) target: HerdrTarget,
    /// The pane's physical writer `socket_path#pane_id`, as the Herdr gate names it.
    pub(crate) writer: String,
    /// The logical session the owner's history names.
    pub(crate) session: String,
    pub(crate) transcript: PathBuf,
    pub(crate) channel: u64,
    pub(crate) source: String,
    pub(crate) author: String,
    pub(crate) name: String,
    pub(crate) nonce: String,
    pub(crate) text: String,
}

impl BusyClaudeHerdr {
    pub(crate) fn leaf(self) -> Leaf {
        Box::new(move |frame| {
            let (source, author, name) = (&self.source, &self.author, &self.name);
            if busy_inject::frame(source, author, name, &self.nonce, &self.text) != frame {
                owner_input::vetoed("frame_mismatch");
                return Ok(Completion::NotSent("frame_mismatch".into()));
            }
            if let Err(refused) = owner_input::bind_writer(&self.writer) {
                owner_input::vetoed(&refused);
                return Ok(Completion::NotSent(refused));
            }
            let input = BusyInput {
                channel: self.channel,
                provider: "claude".into(),
                source: self.source.clone(),
                author: self.author.clone(),
                name: self.name.clone(),
                text: self.text.clone(),
                nonce: self.nonce.clone(),
            };
            let run = || herdr_busy(&self.target, &self.session, &self.transcript, None, &input);
            Ok(
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)) {
                    Ok(InjectAttempt::NotSent(veto)) => {
                        owner_input::vetoed(veto);
                        Completion::NotSent(veto.into())
                    }
                    Ok(InjectAttempt::Injected { .. }) => Completion::Submitted,
                    // Health recorded this failure's one unconfirmed event already.
                    Ok(InjectAttempt::Unconfirmed { detail, .. }) => {
                        Completion::WriteUncertain(detail.into())
                    }
                    Ok(other) => Completion::WriteUncertain(format!("{other:?}")),
                    Err(_) => Completion::WriteUncertain(self.panicked()),
                },
            )
        })
    }

    /// The one alert of an effect that died before health could report it.
    fn panicked(&self) -> String {
        let detail = "executor_failed";
        let payload = serde_json::json!({
            "detail": detail,
            "nonce": self.nonce,
            "source": self.source,
            "tmux_session": self.session,
            "owner": true,
            "host": "herdr",
        });
        tracing::warn!(channel_id = self.channel, %payload, "owner busy input unconfirmed");
        crate::services::observability::events::record_simple(
            "busy_inject_unconfirmed",
            Some(self.channel),
            Some("claude"),
            payload,
        );
        detail.into()
    }
}

#[cfg(test)]
#[path = "herdr_tests.rs"]
mod tests;
