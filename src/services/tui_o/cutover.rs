//! Channel ownership uses the immutable boot policy and, for a selected channel, this process's
//! adoption of it.
//! Uncertain identities withhold the body.
//! This is an ownership fact, never delivery evidence; evidence readers must not consult it.

use super::channel_policy::{self, BootChannels, Candidate, Site};
use crate::services::agent_protocol::RuntimeHandoffKind;

mod channel_gate;
pub(crate) mod intake_route;
#[cfg(unix)]
pub(crate) use channel_gate::peek_o_owns_tui_output_for_channel_quiet;
pub(crate) use channel_gate::{
    BodyClaim, BodySend, IdentityError, claim_then_send, claim_then_send_held, o_keeps_body,
    o_owns_tui_output_for_channel_reserving, peek_o_owns_tui_output_for_channel,
    peek_o_owns_tui_output_for_channel_tmux,
};
#[cfg(test)]
pub(crate) use channel_gate::{
    claims_judged, o_owns_tui_output_for_channel, o_owns_tui_output_for_channel_tmux,
};

pub(crate) fn boot_snapshot() -> Option<BootChannels> {
    test_override::with_channels(|snapshot| snapshot.cloned())
}

/// Each boot-snapshot channel with its boot kind and, where this node may host it, its adoption;
/// the writer host derives its channels from this, never from a reloaded config.
pub(crate) fn boot_ownership() -> Vec<(u64, Option<RuntimeHandoffKind>, Option<Candidate>)> {
    let evaluate = |snapshot: Option<&channel_policy::BootChannels>| {
        let Some(snapshot) = snapshot else {
            return Vec::new();
        };
        let channels = snapshot.channels();
        let judged = |&channel: &u64| {
            let kind = snapshot.kind(channel);
            let hosted = channel_policy::owns_output(channels, channel, kind);
            let blocked = snapshot
                .execution(channel)
                .is_some_and(|view| view.permit.is_err());
            let candidate = snapshot
                .output_candidate(channel)
                .filter(|_| hosted && !blocked);
            (channel, kind, candidate)
        };
        channels.iter().map(judged).collect()
    };
    test_override::with_channels(evaluate)
}

/// The channels O owns with their boot kind.
fn owned_channels() -> Vec<(u64, RuntimeHandoffKind)> {
    test_override::with_channels(|snapshot| {
        let Some(snapshot) = snapshot else {
            return Vec::new();
        };
        let owned = |&channel: &u64| Some((channel, owned_in(snapshot, channel)?));
        snapshot.channels().iter().filter_map(owned).collect()
    })
}

/// The boot kind of `channel` when O owns it; no other channel's adoption is read.
fn owned_kind(channel: u64) -> Option<RuntimeHandoffKind> {
    test_override::with_channels(|snapshot| owned_in(snapshot?, channel))
}

fn owned_in(snapshot: &BootChannels, channel: u64) -> Option<RuntimeHandoffKind> {
    let kind = snapshot.kind(channel);
    if snapshot.execution(channel).is_some() {
        return kind;
    }
    let hosted = channel_policy::owns_output(snapshot.channels(), channel, kind);
    snapshot
        .output_candidate(channel)
        .filter(|c| hosted && c.peek().owned())?;
    kind
}

/// The boot turn selection while a selected Codex channel O owns has not confirmed turn mode.
pub(crate) fn codex_turn_awaited(
    channel: u64,
) -> Option<crate::services::tui_o::turn_mode::TurnConfig> {
    if owned_kind(channel) != Some(RuntimeHandoffKind::CodexTui)
        || super::turn_mode::transcript_turns(channel)
    {
        return None;
    }
    let turn = test_override::with_channels(|snapshot| snapshot.map(|s| s.turn().clone()));
    turn.filter(|turn| turn.selects(channel))
}

/// Whether the boot turn selection names `channel`, so a Codex input there may wait for turn mode.
pub(crate) fn turn_selected(channel: u64) -> bool {
    test_override::with_channels(|snapshot| snapshot.is_some_and(|s| s.turn().selects(channel)))
}

/// Fresh local Codex admission precedes provider effects.
pub(crate) fn mark_codex_turn_admitted(
    channel: u64,
    provider: &crate::services::provider::ProviderKind,
    provider_session_id: Option<&str>,
) {
    use super::writer::binding::{BindingEvents, ChannelBindingLog};
    if *provider != crate::services::provider::ProviderKind::Codex || provider_session_id.is_some()
    {
        return;
    }
    test_override::with_channels(|snapshot| {
        let Some(snapshot) = snapshot.filter(|s| {
            let local = s
                .execution(channel)
                .map_or(s.site() == &Site::Home, |view| view.permit.is_ok());
            local && s.kind(channel) == Some(RuntimeHandoffKind::CodexTui)
        }) else {
            return;
        };
        if let Some(candidate) = snapshot.output_candidate(channel) {
            candidate.mark_turn_admitted(channel, || {
                ChannelBindingLog::new(channel, super::shadow::ShadowProvider::Codex)
                    .binding_events_since(channel, 0)
                    .map(|events| events.last().map_or(0, |event| event.seq))
            });
        }
    });
}

/// Before a new placement: a non-home node holds a selected channel, and the home releases a
/// pending adoption so the placed Legacy turn never shares the channel with O.
fn claim_for_placement(channel: Option<u64>) -> Result<(), String> {
    test_override::with_channels(|snapshot| {
        let Some(snapshot) = snapshot.filter(|s| !s.channels().is_empty()) else {
            return Ok(());
        };
        // An unparseable destination may be any selected channel.
        if channel.is_some_and(|channel| !snapshot.channels().contains(&channel)) {
            return Ok(());
        }
        if let Some(channel) = channel
            && let Some(view) = snapshot.execution(channel)
        {
            return view.permit.map(|_| ());
        }
        if let Site::Foreign { home } = snapshot.site() {
            let channel = channel.map_or("an unknown channel".into(), |c| c.to_string());
            return Err(format!(
                "O channel {channel} is served only on O home {home}"
            ));
        }
        if let Some(channel) = channel
            && let Some(candidate) = snapshot.candidate(channel)
        {
            candidate.claim(channel);
        }
        Ok(())
    })
}

#[cfg(not(test))]
mod test_override {
    use super::channel_policy::{self, BootChannels};

    pub(super) fn with_channels<R>(evaluate: impl FnOnce(Option<&BootChannels>) -> R) -> R {
        evaluate(channel_policy::boot())
    }
}

/// Test builds force the boot channel snapshot per thread or for a whole child process.
#[cfg(test)]
pub(crate) mod test_override {
    use crate::services::agent_protocol::RuntimeHandoffKind;

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
    use crate::services::tui_o::channel_policy::{self, Adoption, BootChannels};
    use std::cell::RefCell;

    pub(crate) const CHANNELS_ENV: &str = "ADK_TEST_O_TUI_CHANNELS";
    thread_local! {
        static CHANNELS: RefCell<Option<BootChannels>> = const { RefCell::new(None) };
    }

    /// Selected channels read as committed, as if each already had its store.
    fn snapshot(channels: &[(u64, RuntimeHandoffKind)]) -> BootChannels {
        unadopted(channels).adopted(Adoption::Committed)
    }

    fn unadopted(channels: &[(u64, RuntimeHandoffKind)]) -> BootChannels {
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
            "server": {}, "agents": agents,
            "tui_o": {"writer": {"channels": ids, "all_tui": false}, "turn": {"all_owned": false}}
        }))
        .unwrap();
        BootChannels::validate(&config).unwrap()
    }

    pub(crate) struct ChannelsGuard {
        previous: Option<BootChannels>,
    }

    pub(crate) fn force_channels(channels: &[(u64, RuntimeHandoffKind)]) -> ChannelsGuard {
        force_snapshot(snapshot(channels))
    }

    /// Selected channels on the home with no store yet: each adoption starts pending.
    pub(crate) fn force_candidates(channels: &[(u64, RuntimeHandoffKind)]) -> ChannelsGuard {
        force_snapshot(unadopted(channels).adopted(Adoption::Pending))
    }

    /// Selected channels as a node other than the O home `home` sees them.
    pub(crate) fn force_foreign(
        channels: &[(u64, RuntimeHandoffKind)],
        home: &str,
    ) -> ChannelsGuard {
        force_snapshot(unadopted(channels).foreign(home))
    }

    /// Installs a booted snapshot, as `channel_policy::install` would leave it.
    pub(crate) fn force_boot(snapshot: BootChannels) -> ChannelsGuard {
        force_snapshot(snapshot)
    }

    /// This thread's forced channels for another thread, over the same candidate locks.
    pub(crate) fn shared_channels() -> impl FnOnce() -> ChannelsGuard + Send + 'static {
        let snapshot = CHANNELS.with(|cell| cell.borrow().clone());
        move || force_snapshot(snapshot.expect("channels are forced on this thread"))
    }

    fn force_snapshot(snapshot: BootChannels) -> ChannelsGuard {
        ChannelsGuard {
            previous: CHANNELS.with(|cell| cell.replace(Some(snapshot))),
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

    /// The owned set matches boot ownership in every adoption state, on either site.
    #[test]
    fn owned_channels_match_boot_ownership() {
        let channels = [
            (41, RuntimeHandoffKind::ClaudeTui),
            (42, RuntimeHandoffKind::CodexTui),
        ];
        let from_boot = || -> Vec<(u64, RuntimeHandoffKind)> {
            let owned = |(channel, kind, candidate): (u64, _, Option<super::Candidate>)| {
                candidate.filter(|c| c.peek().owned())?;
                Some((channel, kind?))
            };
            super::boot_ownership()
                .into_iter()
                .filter_map(owned)
                .collect()
        };
        let states = [
            Adoption::Pending,
            Adoption::Committed,
            Adoption::Held,
            Adoption::Released,
            Adoption::Deferred,
        ];
        let mut owned_somewhere = false;
        for state in states {
            let boots = [
                unadopted(&channels).adopted(state),
                unadopted(&channels).foreign("gw"),
            ];
            for boot in boots {
                let _boot = force_boot(boot);
                assert_eq!(super::owned_channels(), from_boot(), "{state:?}");
                owned_somewhere |= !super::owned_channels().is_empty();
            }
        }
        assert!(owned_somewhere, "some state owns channels");
    }
}
