//! KRX (claude) test files originally declared under `cfg(all(test, unix))` that call discord-layer
//! items; a separate host keeps that single cfg predicate so the declaration key stays the same.

#[path = "../claude_tui/hosting/fresh_hold_tests.rs"]
mod claude_fresh_hold;
#[path = "../claude_tui/hook_server/codex_ingress_tests.rs"]
mod claude_tui_hook_server_codex_ingress;
