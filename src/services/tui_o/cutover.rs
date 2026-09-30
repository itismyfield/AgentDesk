//! Channel ownership uses the immutable boot policy; uncertain identities withhold the body.
//! This is an ownership fact, never delivery evidence; evidence readers must not consult it.

/// The one O writer build switch, shared with the intake topology so both flip together.
/// On, only the channels the boot list selects move to O; an empty list leaves all to Legacy.
pub(crate) use super::topology::O_TUI_WRITER;

use super::channel_policy;
use crate::services::agent_protocol::RuntimeHandoffKind;

mod channel_gate;
pub(crate) mod intake_route;
pub(crate) use channel_gate::{
    IdentityError, o_owns_tui_output_for_channel, o_owns_tui_output_for_channel_tmux,
};

/// Whether the writer switch is on; test builds may turn it on or off per thread.
pub(crate) fn writer_enabled() -> bool {
    test_override::enabled()
}

/// Each boot-snapshot channel with its boot kind and whether O owns its output; the writer host
/// derives its store and actor switches from this, never from a reloaded config.
pub(crate) fn boot_ownership() -> Vec<(u64, Option<RuntimeHandoffKind>, bool)> {
    let enabled = writer_enabled();
    let evaluate = |snapshot: Option<&channel_policy::BootChannels>| {
        let Some(snapshot) = snapshot else {
            return Vec::new();
        };
        let channels = snapshot.channels();
        let judged = |&channel: &u64| {
            let kind = snapshot.kind(channel);
            let owned = channel_policy::owns_output(enabled, channels, channel, kind);
            (channel, kind, owned)
        };
        channels.iter().map(judged).collect()
    };
    test_override::with_channels(evaluate)
}

/// The channels O owns with their boot kind; the snapshot is not read while the writer is off.
fn owned_channels() -> Vec<(u64, RuntimeHandoffKind)> {
    if !writer_enabled() {
        return Vec::new();
    }
    let owned = boot_ownership().into_iter().filter(|&(_, _, owned)| owned);
    owned
        .filter_map(|(channel, kind, _)| Some((channel, kind?)))
        .collect()
}

#[cfg(not(test))]
mod test_override {
    use super::channel_policy::{self, BootChannels};

    pub(super) fn enabled() -> bool {
        super::O_TUI_WRITER
    }

    pub(super) fn with_channels<R>(evaluate: impl FnOnce(Option<&BootChannels>) -> R) -> R {
        evaluate(channel_policy::boot())
    }
}

/// Test builds can act as if the flag were on, per thread or for a whole child process,
/// without touching the constant.
#[cfg(test)]
pub(crate) mod test_override {
    use crate::services::agent_protocol::RuntimeHandoffKind;
    use std::cell::Cell;

    /// Set only on re-exec'd child test processes that own their whole runtime.
    pub(crate) const CHILD_ENV: &str = "ADK_TEST_O_TUI_WRITER";

    thread_local! {
        static FORCED: Cell<Option<bool>> = const { Cell::new(None) };
    }

    /// The switch this thread forced, otherwise the build constant.
    pub(crate) fn enabled() -> bool {
        let child = std::env::var_os(CHILD_ENV).is_some();
        FORCED
            .with(Cell::get)
            .unwrap_or(super::O_TUI_WRITER || child)
    }

    pub(crate) struct ForceGuard(Option<bool>);

    // Private: the list is set with the switch; use `force_channels`.
    fn force_on() -> ForceGuard {
        ForceGuard(FORCED.with(|cell| cell.replace(Some(true))))
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
                runtime_kind: RuntimeHandoffKind::ClaudeTui,
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

    fn snapshot(channels: &[(u64, RuntimeHandoffKind)]) -> BootChannels {
        let agents: Vec<_> = channels
            .iter()
            .map(|(id, kind)| {
                let provider = match kind {
                    RuntimeHandoffKind::ClaudeTui => "claude",
                    RuntimeHandoffKind::CodexTui => "codex",
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

    pub(crate) fn force_channels(channels: &[(u64, RuntimeHandoffKind)]) -> ChannelsGuard {
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
                let entries = serde_json::from_str::<Vec<(u64, RuntimeHandoffKind)>>(&raw).unwrap();
                return evaluate(Some(&snapshot(&entries)));
            }
            // Uninstalled stays None as in production; fixtures that need the empty list set it.
            evaluate(channel_policy::boot())
        })
    }

    /// Re-runs `name` in a child whose whole process reads the empty writer list, for bodies that
    /// reach the gate from runtime worker threads; returns whether this is that child.
    pub(crate) fn in_empty_list_process(name: &str) -> bool {
        if std::env::var_os(CHANNELS_ENV).is_some() {
            return true;
        }
        let qualified = name.split_once("::").unwrap().1;
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", qualified, "--nocapture"])
            .env(CHANNELS_ENV, "[]")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stdout}\n{stderr}");
        assert!(
            stdout.contains("1 passed; 0 failed; 0 ignored;"),
            "{stdout}"
        );
        false
    }

    pub(crate) fn force_off() -> ForceGuard {
        ForceGuard(FORCED.with(|cell| cell.replace(Some(false))))
    }
    // Registry-reset tests run concurrently, so binding-dependent fixtures use their own process.
    pub(crate) fn isolated_binding_case(name: &str) -> bool {
        const CHILD: &str = "ADK_TEST_O_BINDING_CASE";
        if std::env::var(CHILD).as_deref() == Ok(name) {
            return true;
        }
        let qualified = name.split_once("::").unwrap().1;
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", qualified, "--nocapture"])
            .env(CHILD, name)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed; 0 ignored;"));
        false
    }
}
