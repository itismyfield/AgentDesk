//! Channel ownership uses the immutable boot policy; uncertain identities withhold the body.
//! This is an ownership fact, never delivery evidence; evidence readers must not consult it.

use crate::services::agent_protocol::RuntimeHandoffKind;

/// The one O writer build switch, shared with the intake topology so both flip together.
/// While false every guard below is a no-op.
pub(crate) use super::topology::O_TUI_WRITER;

mod channel_gate;
pub(crate) use channel_gate::{
    IdentityError, o_owns_tui_output_for_channel, o_owns_tui_output_for_channel_tmux,
};

// Channel-free callers must migrate before an enabled build can compile.
const _: () = assert!(!O_TUI_WRITER);

/// Channel-free compatibility helper; enabled builds must remove this API.
pub(crate) fn o_owns_tui_output(kind: Option<RuntimeHandoffKind>) -> bool {
    o_owns_tui_output_with(O_TUI_WRITER || test_override::forced(), kind)
}

pub(crate) fn o_owns_tui_output_with(enabled: bool, kind: Option<RuntimeHandoffKind>) -> bool {
    enabled
        && matches!(
            kind,
            Some(RuntimeHandoffKind::ClaudeTui | RuntimeHandoffKind::CodexTui)
        )
}

/// Same decision keyed by tmux session: runtime binding first, then the session kind marker.
pub(crate) fn o_owns_tui_output_for_tmux_session(tmux_session_name: &str) -> bool {
    if !(O_TUI_WRITER || test_override::forced()) {
        return false;
    }
    let kind =
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(tmux_session_name)
            .map(|binding| binding.runtime_kind)
            .or_else(|| {
                crate::services::tmux_common::resolve_tmux_runtime_kind_marker(tmux_session_name)
            });
    o_owns_tui_output(kind)
}

#[cfg(not(test))]
mod test_override {
    pub(super) fn forced() -> bool {
        false
    }
}

/// Test builds can act as if the flag were on, per thread or for a whole child process,
/// without touching the constant.
#[cfg(test)]
pub(crate) mod test_override {
    use std::cell::Cell;

    /// Set only on re-exec'd child test processes that own their whole runtime.
    pub(crate) const CHILD_ENV: &str = "ADK_TEST_O_TUI_WRITER";

    thread_local! {
        static FORCED: Cell<bool> = const { Cell::new(false) };
    }

    pub(crate) fn forced() -> bool {
        FORCED.with(Cell::get) || std::env::var_os(CHILD_ENV).is_some()
    }

    pub(crate) struct ForceGuard(bool);

    pub(crate) fn force_on() -> ForceGuard {
        ForceGuard(FORCED.with(|cell| cell.replace(true)))
    }

    impl Drop for ForceGuard {
        fn drop(&mut self) {
            FORCED.with(|cell| cell.set(self.0));
        }
    }

    /// Binds a tmux session as a Claude TUI until dropped, so session-keyed gates see a TUI kind.
    pub(crate) struct TuiSessionGuard(String);

    pub(crate) fn bind_claude_tui_session(
        tmux_session: &str,
        output_path: &str,
    ) -> TuiSessionGuard {
        crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
            tmux_session,
            crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
                runtime_kind: super::RuntimeHandoffKind::ClaudeTui,
                output_path: output_path.to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: None,
                last_offset: 0,
                relay_last_offset: None,
            },
        );
        TuiSessionGuard(tmux_session.to_string())
    }

    impl Drop for TuiSessionGuard {
        fn drop(&mut self) {
            crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&self.0);
        }
    }
    use crate::services::tui_o::channel_policy::{self, BootChannels};
    use std::cell::RefCell;

    pub(crate) const CHANNELS_ENV: &str = "ADK_TEST_O_TUI_CHANNELS";
    thread_local! {
        static CHANNELS: RefCell<Option<BootChannels>> = const { RefCell::new(None) };
    }

    fn snapshot(channels: &[(u64, super::RuntimeHandoffKind)]) -> BootChannels {
        let agents: Vec<_> = channels
            .iter()
            .map(|(id, kind)| {
                let provider = match kind {
                    super::RuntimeHandoffKind::ClaudeTui => "claude",
                    super::RuntimeHandoffKind::CodexTui => "codex",
                    _ => panic!("test writer channel must be TUI"),
                };
                serde_json::json!({"id": format!("writer-{id}"), "name": "Writer", "channels": {
                    provider: {"id": id.to_string(), "runtime": "tui"}
                }})
            })
            .collect();
        let ids: Vec<_> = channels.iter().map(|(id, _)| *id).collect();
        let config = serde_json::from_value(serde_json::json!({
            "server": {}, "agents": agents, "tui_o": {"writer": {"channels": ids}}
        }))
        .unwrap();
        BootChannels::validate(&config).unwrap()
    }

    pub(crate) struct ChannelsGuard {
        _forced: ForceGuard,
        previous: Option<BootChannels>,
    }

    pub(crate) fn force_channels(channels: &[(u64, super::RuntimeHandoffKind)]) -> ChannelsGuard {
        ChannelsGuard {
            _forced: force_on(),
            previous: CHANNELS.with(|cell| cell.replace(Some(snapshot(channels)))),
        }
    }

    impl Drop for ChannelsGuard {
        fn drop(&mut self) {
            CHANNELS.with(|cell| cell.replace(self.previous.take()));
        }
    }

    pub(crate) fn with_channels<R>(evaluate: impl FnOnce(Option<&BootChannels>) -> R) -> R {
        CHANNELS.with(|cell| {
            let value = cell.borrow();
            if let Some(snapshot) = value.as_ref() {
                return evaluate(Some(snapshot));
            }
            if let Ok(raw) = std::env::var(CHANNELS_ENV) {
                let entries =
                    serde_json::from_str::<Vec<(u64, super::RuntimeHandoffKind)>>(&raw).unwrap();
                return evaluate(Some(&snapshot(&entries)));
            }
            evaluate(channel_policy::boot())
        })
    }

    pub(crate) fn force_off() -> ForceGuard {
        ForceGuard(FORCED.with(|cell| cell.replace(false)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn o_tui_writer_is_off_until_flip() {
        assert!(!O_TUI_WRITER);
        assert!(!o_owns_tui_output(Some(RuntimeHandoffKind::ClaudeTui)));
    }

    #[test]
    fn o_owns_tui_output_truth_table() {
        let kinds = [
            (Some(RuntimeHandoffKind::ClaudeTui), true),
            (Some(RuntimeHandoffKind::CodexTui), true),
            (Some(RuntimeHandoffKind::LegacyTmuxWrapper), false),
            (Some(RuntimeHandoffKind::ProcessBackend), false),
            (Some(RuntimeHandoffKind::ClaudeEAdapter), false),
            (None, false),
        ];
        for (kind, tui) in kinds {
            assert!(!o_owns_tui_output_with(false, kind), "{kind:?} off");
            assert_eq!(o_owns_tui_output_with(true, kind), tui, "{kind:?} on");
        }
    }
}
