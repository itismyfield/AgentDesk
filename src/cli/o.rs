//! `agentdesk o`: operator actions on the O writer's store.

use clap::{Args, Subcommand};

use crate::services::tui_o::store::OStore;
use crate::services::tui_o::store::rotation::ResolveFrom;

#[derive(Args)]
#[command(about = "O writer store operations: resolve a pending source boundary")]
pub(crate) struct OArgs {
    #[command(subcommand)]
    command: OCommand,
}

#[derive(Subcommand)]
pub(crate) enum OCommand {
    /// Source boundaries the writer could not decide
    #[command(subcommand)]
    Boundary(BoundaryCommand),
}

#[derive(Subcommand)]
pub(crate) enum BoundaryCommand {
    /// Record where a pending source's owed records start; the writer applies it at its next start
    Resolve {
        #[arg(long)]
        channel: u64,
        /// The pending source's spool key or transcript path
        #[arg(long)]
        source: String,
        /// A byte offset at a record start, or the uuid of the first owed record
        #[arg(long)]
        from: String,
        #[arg(long, default_value = "operator")]
        operator: String,
    },
}

pub(crate) fn run(args: OArgs) -> Result<(), String> {
    let OCommand::Boundary(BoundaryCommand::Resolve {
        channel,
        source,
        from,
        operator,
    }) = args.command;
    let runtime_root = crate::config::runtime_root().ok_or("runtime root is unresolved")?;
    let store = OStore::existing(&runtime_root).ok_or("no O store under the runtime root")?;
    let from = match from.parse::<u64>() {
        Ok(offset) => ResolveFrom::Offset(offset),
        Err(_) => ResolveFrom::Uuid(from),
    };
    let recorded = store.record_boundary_resolved(channel, &source, &from, &operator);
    let (source, from) = recorded.map_err(|error| format!("boundary resolve: {error:?}"))?;
    let path = source.path.display();
    println!("{path} is owed from byte {from}; the writer applies it at its next start");
    Ok(())
}
