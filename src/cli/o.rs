//! Operator commands for inspecting and applying transcript transitions.

use clap::{Args, Subcommand};

#[path = "o_transition.rs"]
mod transition;

#[derive(Args)]
#[command(about = "Inspect and apply O writer transcript transitions")]
pub(crate) struct OArgs {
    #[command(subcommand)]
    command: OCommand,
}

#[derive(Subcommand)]
pub(crate) enum OCommand {
    /// Ask the local server about a transcript transition's cleanup; opens no store here
    #[command(subcommand)]
    Transition(transition::TransitionCommand),
}

pub(crate) fn run(args: OArgs) -> Result<(), String> {
    match args.command {
        OCommand::Transition(command) => transition::run(command),
    }
}

#[cfg(test)]
mod removal_tests {
    use clap::{CommandFactory, Parser, error::ErrorKind};

    #[derive(Parser)]
    struct Cli {
        #[command(subcommand)]
        command: crate::cli::args::Commands,
    }

    #[test]
    fn retired_o_store_commands_are_rejected() {
        for args in [
            vec!["agentdesk", "o", "status", "--channel", "7"],
            vec![
                "agentdesk",
                "o",
                "resume",
                "--channel",
                "7",
                "--rejected-serial",
                "0",
                "--reason",
                "restored",
            ],
            vec![
                "agentdesk",
                "o",
                "boundary",
                "resolve",
                "--channel",
                "7",
                "--source",
                "old",
                "--from",
                "0",
            ],
        ] {
            let result = Cli::command().try_get_matches_from(&args);
            assert!(result.is_err(), "retired command accepted: {args:?}");
            assert_eq!(result.unwrap_err().kind(), ErrorKind::InvalidSubcommand);
        }
    }
}
