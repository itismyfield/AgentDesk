#![recursion_limit = "256"]

use anyhow::Result;

fn main() -> Result<()> {
    agentdesk::run_from_args()
}
