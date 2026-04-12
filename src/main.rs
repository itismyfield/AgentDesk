#![recursion_limit = "256"]
mod bootstrap;
mod cli;
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

#[cfg(test)]
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
