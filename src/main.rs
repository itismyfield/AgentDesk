#![recursion_limit = "256"]
#![deny(clippy::drop_non_drop)]
#![deny(clippy::await_holding_lock)]
#![deny(clippy::mistyped_literal_suffixes)]
#![deny(clippy::correctness)]

#[cfg(test)]
include!("config/runtime_root_guard/test_process.rs");

use anyhow::Result;

fn main() -> Result<()> {
    agentdesk::run_from_args()
}
