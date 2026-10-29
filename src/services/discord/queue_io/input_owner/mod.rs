//! The common TUI input owner as callers outside the actor see it; test-only until its ingress is
//! wired. Each later slice adds its own `mod` line here and nothing else.
pub(crate) mod admission;
pub(crate) mod effect;
pub(crate) mod ingress;
pub(crate) mod origin;
pub(crate) mod pump;
pub(crate) mod target;
