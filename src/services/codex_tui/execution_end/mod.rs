#[cfg(unix)]
pub(crate) mod attempt;
#[cfg(unix)]
pub(crate) mod capture;
#[cfg(unix)]
pub(crate) mod confirm;
pub(crate) mod identity;
#[cfg(unix)]
pub(crate) mod producer;

#[cfg(all(test, unix))]
mod attempt_tests;

/// Test-only fault injection; release builds never consult the environment.
#[cfg(all(test, unix))]
pub(crate) fn mutant(name: &str) -> bool {
    std::env::var("ADK_CODEX_END_MUTANT").is_ok_and(|armed| armed == name)
}

#[cfg(all(not(test), unix))]
pub(crate) fn mutant(_name: &str) -> bool {
    false
}
