#![recursion_limit = "256"]
#![deny(clippy::drop_non_drop)]

#[cfg(test)]
include!("config/runtime_root_guard/test_process.rs");

use anyhow::Result;

fn main() -> Result<()> {
    agentdesk::run_from_args()
}
