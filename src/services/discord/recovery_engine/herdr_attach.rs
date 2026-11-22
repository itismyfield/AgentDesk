//! What restoring a Herdr pane's source left behind, read by the restart and terminate paths.

use super::host_reconcile::HostReconcile;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) enum HerdrSourceAttach {
    /// The row holds no hosted execution; the tmux path keeps it.
    NotHerdr,
    /// The reconcile did not admit this execution; nothing was registered.
    Refused(HostReconcile),
    /// The log names no source of this execution to restore.
    NoBaseline,
    /// The execution's own canonical clear waits on its new session: input is admitted and no
    /// source or cursor is restored until that session's record lands.
    AwaitingClear,
    /// The binding event was not persisted, the log refused it, or another source is live there;
    /// nothing was published.
    NotPublished,
    /// `bound` once the pane's latest logged source is this execution's. `agent_agrees` compares
    /// Herdr's reported agent session with that source: a hint for a re-read, never a switch.
    Published {
        bound: bool,
        agent_agrees: Option<bool>,
    },
}
