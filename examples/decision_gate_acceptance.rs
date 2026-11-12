#![cfg(not(test))]

#[cfg(unix)]
#[path = "decision_gate_acceptance/harness.rs"]
pub mod harness;
#[cfg(unix)]
pub use harness::services;
#[cfg(unix)]
pub(crate) use harness::{config, github, issue_duplicate, transport, utils};

fn main() {
    #[cfg(unix)]
    harness::main();
    #[cfg(not(unix))]
    {
        eprintln!("HARNESS_FAILED reason=unix_required");
        std::process::exit(1);
    }
}
