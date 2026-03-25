use std::fmt;

/// Lifecycle hooks that policies can register handlers for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Hook {
    OnSessionStatusChange,
    OnCardTransition,
    OnCardTerminal,
    OnDispatchCompleted,
    OnReviewEnter,
    OnReviewVerdict,
    OnTick,
    /// Fast tick (30s) — retry, unsent notification recovery
    OnTick30s,
    /// Normal tick (1m) — timeouts, orphan recovery, stale detection
    OnTick1min,
    /// Slow tick (5m) — reconciliation, deadlock, context check
    OnTick5min,
}

impl Hook {
    /// The JS property name used when registering this hook in a policy object.
    pub fn js_name(&self) -> &'static str {
        match self {
            Hook::OnSessionStatusChange => "onSessionStatusChange",
            Hook::OnCardTransition => "onCardTransition",
            Hook::OnCardTerminal => "onCardTerminal",
            Hook::OnDispatchCompleted => "onDispatchCompleted",
            Hook::OnReviewEnter => "onReviewEnter",
            Hook::OnReviewVerdict => "onReviewVerdict",
            Hook::OnTick => "onTick",
            Hook::OnTick30s => "onTick30s",
            Hook::OnTick1min => "onTick1min",
            Hook::OnTick5min => "onTick5min",
        }
    }

    /// All known hooks.
    pub fn all() -> &'static [Hook] {
        &[
            Hook::OnSessionStatusChange,
            Hook::OnCardTransition,
            Hook::OnCardTerminal,
            Hook::OnDispatchCompleted,
            Hook::OnReviewEnter,
            Hook::OnReviewVerdict,
            Hook::OnTick,
            Hook::OnTick30s,
            Hook::OnTick1min,
            Hook::OnTick5min,
        ]
    }

    /// Parse a hook name string back into a Hook variant.
    pub fn from_str(s: &str) -> Option<Hook> {
        Hook::all().iter().find(|h| h.js_name() == s).copied()
    }
}

impl fmt::Display for Hook {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.js_name())
    }
}
