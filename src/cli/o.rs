//! `agentdesk o`: operator actions on the O writer's store.

use std::path::Path;

use clap::{Args, Subcommand};

use crate::services::tui_o::store::ledger::{PieceOutcome, PieceRecord};
use crate::services::tui_o::store::rotation::ResolveFrom;
use crate::services::tui_o::store::{
    OPERATOR_RESUME_FLOOR, OStore, ResumeRecord, STORE_DIR_NAME, StoreError,
};

#[derive(Args)]
#[command(
    about = "O writer store operations: resolve a pending source boundary, approve a rejected piece"
)]
pub(crate) struct OArgs {
    #[command(subcommand)]
    command: OCommand,
}

#[derive(Subcommand)]
pub(crate) enum OCommand {
    /// Source boundaries the writer could not decide
    #[command(subcommand)]
    Boundary(BoundaryCommand),
    /// Rejected pieces of a channel and their operator approvals; writes nothing
    Status {
        #[arg(long)]
        channel: u64,
    },
    /// Approve one more POST of the latest 400/403/404 piece; the writer sends it at its next start
    #[command(
        after_help = "Exit: 0 recorded, 1 no store, 3 already recorded, 4 refused, 5 busy, 6 not written, 7 unknown"
    )]
    Resume {
        #[arg(long)]
        channel: u64,
        #[arg(long)]
        rejected_serial: u64,
        #[arg(long)]
        reason: String,
        #[arg(long, default_value = "operator")]
        operator: String,
    },
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
    let runtime_root = crate::config::runtime_root().ok_or("runtime root is unresolved")?;
    let store = OStore::existing(&runtime_root).ok_or("no O store under the runtime root")?;
    match args.command {
        OCommand::Boundary(BoundaryCommand::Resolve {
            channel,
            source,
            from,
            operator,
        }) => {
            let from = match from.parse::<u64>() {
                Ok(offset) => ResolveFrom::Offset(offset),
                Err(_) => ResolveFrom::Uuid(from),
            };
            let recorded = store.record_boundary_resolved(channel, &source, &from, &operator);
            let (source, from) =
                recorded.map_err(|error| format!("boundary resolve: {error:?}"))?;
            let path = source.path.display();
            println!("{path} is owed from byte {from}; the writer applies it at its next start");
            Ok(())
        }
        OCommand::Status { channel } => status(&store, channel),
        OCommand::Resume {
            channel,
            rejected_serial,
            reason,
            operator,
        } => {
            let (code, message) = resume(
                &store,
                &runtime_root,
                channel,
                rejected_serial,
                &operator,
                &reason,
            );
            if code == 0 {
                println!("{message}");
                return Ok(());
            }
            eprintln!("Error: {message}");
            std::process::exit(code)
        }
    }
}

fn status(store: &OStore, channel: u64) -> Result<(), String> {
    let state = store
        .operator_resume_status(channel)
        .map_err(|error| format!("o status: {error:?}"))?;
    let blocked = state.blocked().map_or("none".into(), |(serial, status)| {
        format!("serial {serial} ({status})")
    });
    println!("channel {channel}: blocked {blocked}");
    for serial in 0..state.next_serial() {
        let Some(piece) = state.piece(serial) else {
            continue;
        };
        let Some(PieceOutcome::Rejected(status)) = piece.outcome else {
            continue;
        };
        let (key, index) = (&piece.unit_key, piece.piece_index);
        let latest = state
            .latest_piece(key, index)
            .map_or("none".into(), |(latest, record)| {
                format!("{latest} {}", outcome(Some(record)))
            });
        let approval = state.approval(serial).map_or("absent".into(), |approval| {
            let consumed = approval
                .consumed_serial
                .map_or("unconsumed".into(), |consumed| {
                    format!(
                        "consumed by serial {consumed} {}",
                        outcome(state.piece(consumed))
                    )
                });
            let (id, by, reason) = (approval.approval_id, &approval.operator, &approval.reason);
            format!("{consumed}, {id} by {by:?} at {}: {reason:?}", approval.at)
        });
        let disposition = state.disposition(key, index);
        println!(
            "serial {serial} rejected {status}: {:?} {} {:?} piece {index}; latest {latest}; \
             {disposition:?}; approval {approval}",
            key.provider, key.native_key, key.kind
        );
    }
    Ok(())
}

fn outcome(piece: Option<&PieceRecord>) -> String {
    let Some(piece) = piece else {
        return "missing".into();
    };
    let outcome = piece.outcome.as_ref();
    outcome.map_or("open".into(), |outcome| format!("{outcome:?}"))
}

/// The exit code and message for one approval attempt; only an approval this call wrote exits 0.
fn resume(
    store: &OStore,
    runtime_root: &Path,
    channel: u64,
    serial: u64,
    operator: &str,
    reason: &str,
) -> (i32, String) {
    let floor = runtime_root
        .join(STORE_DIR_NAME)
        .join(OPERATOR_RESUME_FLOOR);
    #[cfg(all(test, unix))]
    tests::pause_before_entry();
    match store.record_operator_resume_outcome(channel, serial, operator, reason) {
        Ok(ResumeRecord::Recorded(approval)) => {
            let id = approval.approval_id;
            let message = format!(
                "approval {id} for serial {serial} is durable; nothing was sent yet. Run the \
                 managed restart after this command exits; the writer then sends the piece once"
            );
            (0, message)
        }
        Ok(ResumeRecord::Existing(approval)) => {
            let state = approval
                .consumed_serial
                .map_or("unconsumed".into(), |consumed| {
                    format!("consumed by serial {consumed}")
                });
            let (id, by, at, text) = (
                approval.approval_id,
                &approval.operator,
                approval.at,
                &approval.reason,
            );
            let message = format!(
                "serial {serial} already has approval {id} by {by:?} at {at}: {text:?} \
                 ({state}); nothing new was written"
            );
            (3, message)
        }
        Err(StoreError::Rejected(detail)) => (4, format!("refused, nothing was written: {detail}")),
        Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::WouldBlock => {
            let message = "the ledger is locked by a writer or another operator; nothing was \
                           written. Retry once it is released";
            (5, message.into())
        }
        // The floor precedes any approval append, so its absence proves nothing was appended.
        Err(error)
            if std::fs::symlink_metadata(&floor)
                .is_err_and(|missing| missing.kind() == std::io::ErrorKind::NotFound) =>
        {
            (
                6,
                format!("nothing was written, the rollback floor is absent: {error:?}"),
            )
        }
        Err(error) => (
            7,
            format!("the approval may be durable: {error:?}. Run `agentdesk o status` first"),
        ),
    }
}

#[cfg(test)]
#[path = "o_tests.rs"]
pub(crate) mod tests;
