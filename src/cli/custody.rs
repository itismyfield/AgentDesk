//! `adk custody`: read-only views of the boot-custody ledger under the runtime root.

use clap::{Args, Subcommand};

#[derive(Args)]
pub(crate) struct CustodyArgs {
    #[command(subcommand)]
    pub(crate) action: CustodyAction,
}

#[derive(Subcommand)]
pub(crate) enum CustodyAction {
    /// Print each episode's preserved and missing transcript bytes and last attempt.
    Status {
        /// Only this provider's custody directory (for example `claude`).
        #[arg(long)]
        provider: Option<String>,
        /// Only this episode id.
        episode: Option<String>,
    },
}

pub(crate) fn run(args: CustodyArgs) -> Result<(), String> {
    match args.action {
        CustodyAction::Status { provider, episode } => {
            crate::services::discord_custody::cmd_status(provider.as_deref(), episode.as_deref())
        }
    }
}
