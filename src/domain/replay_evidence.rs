//! Closed values shared by replay observation and its sealed evidence.
#[path = "replay_evidence/live.rs"]
pub(crate) mod live;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TerminalKind {
    Done,
    Error,
}
