#![recursion_limit = "256"]

#[cfg(test)]
#[path = "config/runtime_root_guard/test_process.rs"]
mod test_runtime_process;

use anyhow::Result;

fn main() -> Result<()> {
    agentdesk::run_from_args()
}
