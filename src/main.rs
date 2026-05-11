#![recursion_limit = "256"]
// AgentDesk currently keeps CLI/API/test support surfaces in one bin crate.
// Until those move behind library/feature boundaries, dead_code is mostly noise.
#![allow(dead_code)]
mod bootstrap;
mod cli;
pub(crate) mod compat;
mod config;
pub(crate) mod credential;
mod db;
mod dispatch;
mod engine;
mod error;
mod github;
pub(crate) mod kanban;
mod launch;
mod logging;
pub(crate) mod manual_intervention;
pub(crate) mod pipeline;
pub(crate) mod receipt;
pub(crate) mod reconcile;
pub(crate) mod runtime;
pub(crate) mod runtime_layout;
mod server;
mod services;
pub(crate) mod supervisor;
mod ui;
mod utils;
pub(crate) mod voice;

#[cfg(test)]
mod high_risk_recovery;
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod integration_tests;

// Re-export for crate-level access (used by services::discord::mod.rs)
pub(crate) use cli::agentdesk_runtime_root;

use anyhow::{Context, Result};

fn main() -> Result<()> {
    match cli::args::parse() {
        cli::args::ParseOutcome::Command(command) => cli::execute(command),
        cli::args::ParseOutcome::RunServer => {
            let state = bootstrap::initialize().context("Bootstrap failed")?;
            launch::run(state).context("Launch failed")
        }
    }
}
