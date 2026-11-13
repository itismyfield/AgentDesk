//! The common TUI input owner as callers outside the actor see it; test-only until its ingress is
//! wired. Each later slice adds its own `mod` line here and nothing else.
pub(crate) mod admission;
pub(crate) mod claude_handoff;
pub(crate) mod closed;
pub(crate) mod drive;
pub(crate) mod effect;
pub(crate) mod effort_continue;
pub(crate) mod held_watch;
pub(crate) mod ingress;
pub(crate) mod notice;
pub(crate) mod origin;
pub(crate) mod pump;
pub(crate) mod recovery;
pub(crate) mod restart_gate;
pub(crate) mod routine;
pub(crate) mod start;
pub(crate) mod target;

#[cfg(test)]
mod effort_continue_tests;
