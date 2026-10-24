use super::Cli;
use clap::Parser;

#[test]
fn channel_home_is_not_a_cli_subcommand() {
    for subcommand in ["status", "delegate", "reclaim", "force", "abort-release"] {
        let Err(error) = Cli::try_parse_from(["agentdesk", "channel-home", subcommand]) else {
            panic!("removed channel-home command must be rejected by the parser");
        };
        assert_eq!(error.kind(), clap::error::ErrorKind::InvalidSubcommand);
    }
    assert!(Cli::try_parse_from(["agentdesk", "status"]).is_ok());
}
