use clap::{Args, Subcommand};
use serde_json::Value;

/// Inspect first, then explicitly release the returned identity after verifying
/// the provider finished. No age or pane heuristic grants release authority.
#[derive(Debug, Args)]
pub(crate) struct TurnLeaseArgs {
    #[command(subcommand)]
    action: TurnLeaseAction,
}

#[derive(Debug, Subcommand)]
enum TurnLeaseAction {
    /// Read the exact active mailbox lease (JSON, no mutation)
    Inspect {
        #[arg(long)]
        provider: String,
        #[arg(long)]
        channel_id: std::num::NonZeroU64,
    },
    /// Release that lease without stopping or restarting the provider session
    Release {
        /// Exact JSON object returned by inspect; never resolved from current age
        #[arg(long)]
        expected: String,
        /// Operator's explanation of why this turn may be released
        #[arg(long)]
        reason: String,
    },
}

pub(crate) fn run(args: TurnLeaseArgs) -> Result<(), String> {
    let response = match args.action {
        TurnLeaseAction::Inspect {
            provider,
            channel_id,
        } => {
            if !provider
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            {
                return Err("invalid provider".into());
            }
            super::client::get_json(&format!("/api/turn-lease/{provider}/{channel_id}"))?
        }
        TurnLeaseAction::Release { expected, reason } => {
            let expected: Value = serde_json::from_str(&expected).map_err(|e| e.to_string())?;
            if !expected.is_object() || reason.trim().is_empty() {
                return Err(
                    "expected must be an identity object and reason must be nonempty".into(),
                );
            }
            super::client::post_json_value(
                "/api/turn-lease/release",
                serde_json::json!({"expected": expected, "reason": reason}),
            )?
        }
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&response).map_err(|e| e.to_string())?
    );
    Ok(())
}
