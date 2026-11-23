//! KRX (tui_o) test files that call KE items, declared here so they leave the KRX crate.
//! The file stays in place; the `mod` keeps the cfg chain of its original declaration.

#[cfg(unix)]
#[path = "tui_o/writer/fresh_resume_tests/authority_contention_tests.rs"]
mod authority_contention;
