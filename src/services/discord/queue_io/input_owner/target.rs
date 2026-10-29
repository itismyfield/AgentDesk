//! Which effect the owner runs for an attempt and the writer lane it waits on, both read from the
//! verified execution and the target's observed phase, never from a request label.
#![cfg(unix)]
use super::origin::{Execution, InputOrigin};

/// One logical writer (host, root, history's session), nonce left out; a Herdr pane itself is
/// held by the owner's physical writer lease, so two sessions never write one pane at once.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct TargetKey {
    host: String,
    root: String,
    pane: String,
}

impl TargetKey {
    /// The attempt's own target fields, unaltered; an attempt missing any of them, its execution
    /// nonce included, has no lane.
    pub(crate) fn of(attempt: &serde_json::Value) -> Result<Self, String> {
        let target = &attempt["target"];
        let field = |name: &str| {
            (target[name].as_str())
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .ok_or_else(|| format!("attempt target lacks {name}"))
        };
        field("execution_nonce")?;
        Ok(Self {
            host: field("host")?,
            root: field("root")?,
            pane: field("owner")?,
        })
    }
}

/// What the target was observed doing when the attempt was prepared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    /// No live execution: only an actual create may take the input as its first prompt.
    Absent,
    Idle,
    Busy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Route {
    ClaudeBusy,
    ClaudeIdle,
    /// A busy Claude pane hosted on Herdr: health's gated busy transaction.
    ClaudeHerdrBusy,
    /// A busy Codex tmux pane: main's Codex busy transaction aimed at the open model turn.
    CodexBusy,
    /// A busy Codex pane hosted on Herdr: the owner's adapter of that transaction on the gate.
    CodexHerdrBusy,
    /// The first prompt of an execution the provider's launch creates.
    Cold,
    /// An idle Herdr pane's next prompt, written by its driver on the bound execution.
    HerdrIdle,
    /// The owner keeps the original and sends nothing; no other path takes it either.
    Held(&'static str),
}

/// `None` when the owner does not take the input at all; every owned input gets a route.
pub(crate) fn route(
    execution: Execution,
    origin: InputOrigin,
    provider: &str,
    host: &str,
    phase: Phase,
) -> Option<Route> {
    if !execution.owner_takes(origin) {
        return None;
    }
    if execution == Execution::RemoteTui {
        return Some(Route::Held("remote_placement"));
    }
    Some(match (provider, host, phase) {
        ("claude" | "codex", "tmux" | "herdr", Phase::Absent) => Route::Cold,
        ("claude", "tmux", Phase::Busy) => Route::ClaudeBusy,
        ("claude", "tmux", Phase::Idle) => Route::ClaudeIdle,
        ("claude", "herdr", Phase::Busy) => Route::ClaudeHerdrBusy,
        ("codex", "tmux", Phase::Busy) => Route::CodexBusy,
        ("codex", "herdr", Phase::Busy) => Route::CodexHerdrBusy,
        ("claude" | "codex", "herdr", Phase::Idle) => Route::HerdrIdle,
        _ => Route::Held("provider_unsupported"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn attempt(host: &str, pane: &str, nonce: &str) -> serde_json::Value {
        json!({"target":{"host":host,"owner":pane,"root":"/r","execution_nonce":nonce}})
    }

    /// The lane is the writer lease's: a new execution in one pane waits, another pane does not.
    #[test]
    fn the_lane_is_the_physical_writer_and_needs_every_target_field() {
        let a = TargetKey::of(&attempt("tmux", "p1", "n1")).unwrap();
        assert_eq!(a, TargetKey::of(&attempt("tmux", "p1", "n2")).unwrap());
        assert_ne!(a, TargetKey::of(&attempt("tmux", "p2", "n1")).unwrap());
        assert_ne!(a, TargetKey::of(&attempt("herdr", "p1", "n1")).unwrap());
        assert!(TargetKey::of(&attempt("tmux", "p1", "")).is_err());
        assert!(TargetKey::of(&json!({"target":{"host":"tmux","owner":"p"}})).is_err());
    }

    /// A busy Codex Herdr pane is held, never routed to another driver.
    #[test]
    fn only_unknown_providers_are_held() {
        let local = |provider, host, phase| {
            route(
                Execution::LocalTui,
                InputOrigin::Discord,
                provider,
                host,
                phase,
            )
        };
        assert_eq!(
            local("claude", "tmux", Phase::Busy),
            Some(Route::ClaudeBusy)
        );
        assert_eq!(
            local("claude", "tmux", Phase::Idle),
            Some(Route::ClaudeIdle)
        );
        assert_eq!(local("codex", "herdr", Phase::Absent), Some(Route::Cold));
        assert_eq!(
            local("claude", "herdr", Phase::Busy),
            Some(Route::ClaudeHerdrBusy)
        );
        assert_eq!(local("codex", "tmux", Phase::Busy), Some(Route::CodexBusy));
        assert_eq!(
            local("codex", "herdr", Phase::Busy),
            Some(Route::CodexHerdrBusy)
        );
        for provider in ["claude", "codex"] {
            assert_eq!(
                local(provider, "herdr", Phase::Idle),
                Some(Route::HerdrIdle)
            );
        }
        for (provider, host, phase) in [
            ("gemini", "tmux", Phase::Absent),
            ("gemini", "herdr", Phase::Busy),
        ] {
            assert_eq!(
                local(provider, host, phase),
                Some(Route::Held("provider_unsupported")),
                "{provider} {host} {phase:?}"
            );
        }
        assert_eq!(
            route(
                Execution::Process,
                InputOrigin::Routine,
                "claude",
                "tmux",
                Phase::Busy
            ),
            None
        );
    }
}
