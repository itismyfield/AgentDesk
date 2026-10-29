//! `agentdesk build-info`: build switches the deploy scripts read from the binary they are
//! about to install or roll back from, never from Rust source text.

/// Bumped when a field changes meaning; scripts read any other schema as unknown values.
const BUILD_INFO_SCHEMA: u64 = 1;

pub(crate) fn build_info_json() -> serde_json::Value {
    serde_json::json!({
        "schema": BUILD_INFO_SCHEMA,
        "version": env!("CARGO_PKG_VERSION"),
        // CONSUMED-BY: scripts/deploy-release.sh (`_binary_build_flag … o_tui_writer`).
        // Every build runs the O writer for its boot-selected channels.
        "o_tui_writer": true,
        // CONSUMED-BY: scripts/deploy-release.sh (`_binary_build_flag … o_ledger_operator_resume`).
        // Every build includes the ledger reader and the guarded writer consumer.
        "o_ledger_operator_resume": true,
        // CONSUMED-BY: scripts/deploy-release.sh (`_rollback_would_revert_o_spool`).
        "o_spool_write_version": crate::services::tui_o::shadow::IDENTITY_VERSION,
        "o_spool_readable_versions":
            crate::services::tui_o::store::spool::READABLE_IDENTITY_VERSIONS,
    })
}

pub(crate) fn handle_build_info() {
    println!("{}", build_info_json());
}

#[cfg(test)]
mod tests {
    use super::build_info_json;
    use clap::Parser;

    #[derive(Parser)]
    struct Cli {
        #[command(subcommand)]
        command: crate::cli::args::Commands,
    }

    #[test]
    fn build_info_reports_the_compiled_switches() {
        let info = build_info_json();
        assert_eq!(info["schema"], 1);
        assert_eq!(
            info["o_spool_write_version"],
            crate::services::tui_o::shadow::IDENTITY_VERSION
        );
        assert_eq!(
            info["o_spool_readable_versions"],
            serde_json::json!(crate::services::tui_o::store::spool::READABLE_IDENTITY_VERSIONS)
        );
        assert_eq!(info["o_tui_writer"], true);
        assert_eq!(info["o_ledger_operator_resume"], true);
    }

    #[test]
    fn build_info_is_a_subcommand() {
        let cli = Cli::try_parse_from(["agentdesk", "build-info"]).expect("parse build-info");
        assert!(matches!(cli.command, crate::cli::args::Commands::BuildInfo));
    }
}
