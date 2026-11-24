//! KRX tui_prompt_dedupe tests that call discord-layer items, declared here so they leave the KRX crate.
//! The `#[path]` file stays in place and keeps the cfg chain of its original declaration.

#[path = "../../../tui_prompt_dedupe/runtime_binding/codex_verified_tests.rs"]
mod codex_verified;
