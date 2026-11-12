#![cfg(not(test))]

#[cfg(unix)]
#[path = "decision_gate_acceptance/harness.rs"]
mod harness;
#[cfg(unix)]
pub(crate) use harness::{config, github, issue_duplicate, services, transport, utils};

fn main() {
    #[cfg(unix)]
    harness::main();
    #[cfg(not(unix))]
    {
        eprintln!("HARNESS_FAILED reason=unix_required");
        std::process::exit(1);
    }
}
