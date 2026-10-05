//! `agentdesk herdr`: this node's Herdr-hosted executions, read only, and a retire that only reads
//! the pane before its nonce compare-and-set. It never kills, closes or writes a pane.

use clap::{Args, Subcommand};

#[derive(Args)]
#[command(about = "Herdr-hosted executions on this node: status, and retire of an ended one")]
pub(crate) struct HerdrArgs {
    #[command(subcommand)]
    command: HerdrCommand,
}

#[derive(Subcommand)]
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) enum HerdrCommand {
    /// Each local Herdr row, its pane as read now and the held inputs; changes nothing
    Status,
    /// Retire a channel's execution once its pane is gone or only its shell is left
    Retire {
        /// Discord channel id of the execution
        channel: u64,
    },
}

pub(crate) fn run(args: HerdrArgs) -> Result<(), String> {
    #[cfg(unix)]
    return node::run(args.command);
    #[cfg(not(unix))]
    {
        let _ = args.command;
        Err("agentdesk herdr runs only on unix".into())
    }
}

#[cfg(unix)]
mod node {
    use serde_json::{Value, json};
    use sqlx::PgPool;

    use super::HerdrCommand;
    use crate::db::dispatched_sessions::hosted_execution::{
        HostedCasOutcome, HostedExecution, HostedObservation, HostedRecord, ProcessStamp,
        list_local_herdr_rows_pg, retire_pg,
    };
    use crate::services::claude::herdr_turn::{input_holds, release_hold};
    use crate::services::session_host::{PaneProvider, PaneReading, herdr_endpoints};

    /// Why a retire changed nothing: the row and every hold are as they were.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) enum RetireRefusal {
        NoLocalEndpoint,
        /// No live Herdr row of this channel on this node, or more than one.
        NoRow(&'static str),
        ProviderRunning,
        /// The pane was not read as gone or as its own shell alone; it may still run something.
        Unproven(&'static str),
        /// The row changed after it was read, or the transition was refused.
        Changed(String),
    }

    /// A retired execution and what became of its input hold.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct Retired {
        pub nonce: String,
        pub hold: Result<(), String>,
    }

    pub(super) fn run(command: HerdrCommand) -> Result<(), String> {
        let config = crate::config::load().map_err(|error| format!("load config: {error}"))?;
        crate::config::session_hosts::install(&config).map_err(|error| format!("{error}"))?;
        super::super::direct::run_async(async move {
            let pool = crate::db::postgres::connect(&config)
                .await?
                .ok_or("postgres pool unavailable for herdr")?;
            let result = match command {
                HerdrCommand::Status => status(&pool).await.map(|status| pretty(&status)),
                HerdrCommand::Retire { channel } => match retire(&pool, channel).await {
                    Ok(retired) => Ok(retired_text(channel, &retired)),
                    Err(refusal) => Err(format!("herdr retire {channel} refused: {refusal:?}")),
                },
            };
            pool.close().await;
            println!("{}", result?);
            Ok(())
        })
    }

    fn pretty(value: &Value) -> String {
        serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
    }

    fn retired_text(channel: u64, retired: &Retired) -> String {
        match &retired.hold {
            Ok(()) => format!("channel {channel}: execution {} retired", retired.nonce),
            Err(error) => format!(
                "channel {channel}: execution {} retired; its input hold stays: {error}",
                retired.nonce
            ),
        }
    }

    /// This node's live Herdr rows, read once.
    async fn local_rows(pool: &PgPool) -> Result<Vec<HostedObservation>, RetireRefusal> {
        let node = crate::config::session_hosts::local_node();
        let Some(node) = node.filter(|_| !herdr_endpoints().is_empty()) else {
            return Err(RetireRefusal::NoLocalEndpoint);
        };
        list_local_herdr_rows_pg(pool, &node)
            .await
            .map_err(RetireRefusal::Changed)
    }

    /// The pane of `record` read on its registered endpoint, off the async runtime.
    async fn read_pane(record: &HostedExecution) -> Option<PaneReading> {
        let target = herdr_endpoints().target(record)?;
        let read = tokio::task::spawn_blocking(move || target.read_execution()).await;
        Some(read.unwrap_or_else(|error| PaneReading::Unreadable(error.to_string())))
    }

    fn known(row: &HostedObservation) -> Option<&HostedExecution> {
        match &row.record {
            HostedRecord::Known(record) => Some(record),
            _ => None,
        }
    }

    /// Retires `channel`'s execution only when its pane reads as gone, or as the recorded root shell
    /// with no provider; then that execution's hold, and only that one, is released.
    pub(crate) async fn retire(pool: &PgPool, channel: u64) -> Result<Retired, RetireRefusal> {
        let rows = local_rows(pool).await?;
        let channel = channel.to_string();
        let mut matching = rows
            .iter()
            .filter(|row| known(row).is_some_and(|record| record.owner.channel_id == channel));
        let (Some(row), None) = (matching.next(), matching.next()) else {
            return Err(RetireRefusal::NoRow(
                "no single live herdr row of the channel",
            ));
        };
        let record = known(row).ok_or(RetireRefusal::NoRow("unreadable record"))?;
        let reading = read_pane(record)
            .await
            .ok_or(RetireRefusal::NoLocalEndpoint)?;
        retire_on_reading(pool, row, record, &reading).await
    }

    /// The retire decision on one reading; the hold goes only after the row's CAS wrote Retired.
    pub(crate) async fn retire_on_reading(
        pool: &PgPool,
        row: &HostedObservation,
        record: &HostedExecution,
        reading: &PaneReading,
    ) -> Result<Retired, RetireRefusal> {
        let ended = match reading {
            PaneReading::Missing => true,
            PaneReading::Present {
                root,
                provider: PaneProvider::Exited,
            } => record
                .expected
                .as_ref()
                .is_some_and(|expected| expected.root == *root),
            _ => false,
        };
        if !ended {
            return Err(match reading {
                PaneReading::Present {
                    provider: PaneProvider::Execution(_),
                    ..
                } => RetireRefusal::ProviderRunning,
                PaneReading::Present {
                    provider: PaneProvider::Exited,
                    ..
                } => RetireRefusal::Unproven("root shell replaced"),
                PaneReading::Present { .. } => RetireRefusal::Unproven("provider unverified"),
                _ => RetireRefusal::Unproven("pane unreadable or snapshot incomplete"),
            });
        }
        let nonce = &record.execution_nonce;
        match retire_pg(pool, row, &record.owner, nonce).await {
            Ok(HostedCasOutcome::Written) => Ok(Retired {
                nonce: nonce.clone(),
                hold: release_hold(nonce),
            }),
            Ok(HostedCasOutcome::Stale) => {
                Err(RetireRefusal::Changed("row changed since read".into()))
            }
            Err(error) => Err(RetireRefusal::Changed(format!("{error:?}"))),
        }
    }

    fn pane_text(reading: Option<&PaneReading>, record: &HostedExecution) -> &'static str {
        let root_kept =
            |root: &ProcessStamp| record.expected.as_ref().is_some_and(|e| e.root == *root);
        match reading {
            None => "no_local_endpoint",
            Some(PaneReading::Missing) => "missing",
            Some(PaneReading::Unreadable(_)) => "unreadable",
            Some(PaneReading::Present { provider, root }) => match provider {
                PaneProvider::Execution(_) => "provider_running",
                PaneProvider::Exited if root_kept(root) => "provider_exited",
                PaneProvider::Exited => "root_replaced",
                PaneProvider::Unverified(_) => "provider_unverified",
            },
        }
    }

    /// Every local row with its pane as read now, and every hold by nonce and recorded time; no path
    /// or input text is shown.
    pub(crate) async fn status(pool: &PgPool) -> Result<Value, String> {
        let rows = local_rows(pool)
            .await
            .map_err(|error| format!("{error:?}"))?;
        let mut holds = input_holds()?;
        let mut shown = Vec::new();
        for row in &rows {
            let Some(record) = known(row) else {
                shown.push(json!({"session_row": row.session_id(), "record": "unreadable"}));
                continue;
            };
            let reading = read_pane(record).await;
            let nonce = &record.execution_nonce;
            let held = holds.iter().position(|(held, _)| held == nonce);
            let hold = held.map(|at| json!({"recorded_at": holds.remove(at).1}));
            shown.push(json!({
                "channel": record.owner.channel_id,
                "provider": record.owner.provider,
                "state": record.state,
                "nonce": nonce,
                "pane": pane_text(reading.as_ref(), record),
                "input_hold": hold,
            }));
        }
        let other: Vec<Value> = holds
            .into_iter()
            .map(|(nonce, at)| json!({"nonce": nonce, "recorded_at": at}))
            .collect();
        Ok(json!({"executions": shown, "other_input_holds": other}))
    }
}

#[cfg(all(test, unix))]
use node::{RetireRefusal, Retired, retire, retire_on_reading, status};

#[cfg(test)]
#[path = "herdr_tests.rs"]
mod tests;
