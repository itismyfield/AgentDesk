//! `agentdesk herdr`: this node's Herdr-hosted executions: a read-only status, a retire that only
//! reads the pane before its nonce compare-and-set, a terminate the local server performs, and a
//! read-only probe of the execution user's own server.

use clap::{Args, Subcommand};
use serde_json::{Value, json};

#[derive(Args)]
#[command(
    about = "Herdr-hosted executions on this node: status, retire of an ended one, terminate, and a user-default probe"
)]
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
    /// Ask the local server to close one execution's pane and retire it; needs the live switch
    Terminate {
        /// Session key of the execution's row
        #[arg(long)]
        session_key: String,
        /// Execution nonce the row must still record
        #[arg(long)]
        execution_nonce: String,
        /// Why it is terminated, kept in the server's result log
        #[arg(long)]
        reason: String,
    },
    /// Check the execution user's own herdr server once; reads no AgentDesk config or database
    ProbeUserDefault {
        /// Absolute socket path; defaults to $HOME/.config/herdr/herdr.sock
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
    },
}

pub(crate) fn run(args: HerdrArgs) -> Result<(), String> {
    let command = match args.command {
        HerdrCommand::Terminate {
            session_key,
            execution_nonce,
            reason,
        } => {
            let body = json!({"session_key": session_key, "execution_nonce": execution_nonce, "reason": reason});
            return terminate(&super::client::api_base(), &body);
        }
        #[cfg(unix)]
        HerdrCommand::ProbeUserDefault { socket } => {
            return probe::run(socket, std::env::var_os("HOME"), &mut std::io::stdout());
        }
        command => command,
    };
    #[cfg(unix)]
    return node::run(command);
    #[cfg(not(unix))]
    {
        let _ = command;
        Err("agentdesk herdr runs only on unix".into())
    }
}

/// One request to the local server; only its 200 with `result` `retired` is success.
fn terminate(base: &str, body: &Value) -> Result<(), String> {
    let (status, reply) = super::client::post_json_local_at(base, "/api/herdr/terminate", body)
        .map_err(|error| format!("herdr terminate: no answer from {base}: {error:?}"))?;
    println!(
        "{}",
        serde_json::to_string_pretty(&reply).unwrap_or_else(|_| reply.to_string())
    );
    if status == 200 && reply["result"] == "retired" {
        return Ok(());
    }
    Err(format!(
        "herdr terminate: HTTP {status}, result {}",
        reply["result"]
    ))
}

#[cfg(unix)]
pub(crate) async fn install_observation_hosts(
    config: &crate::config::Config,
    pool: &sqlx::PgPool,
) -> Result<(), String> {
    let candidate = crate::config::session_hosts::BootSessionHosts::from_config(config)
        .map_err(|error| format!("{error}"))?;
    let selection = crate::config::session_hosts::migration::select(
        candidate,
        &crate::config::session_hosts::approval_store::FileApprovalStore::runtime(),
        &crate::config::session_hosts::migration::PgRows(Some(pool)),
        &crate::config::session_hosts::migration::NoHoldStore,
        true,
    )
    .await;
    crate::config::session_hosts::install_selected(selection);
    Ok(())
}

#[cfg(unix)]
mod node {
    use serde_json::{Value, json};
    use sqlx::PgPool;

    use super::HerdrCommand;
    use crate::db::dispatched_sessions::hosted_execution::{
        HostedCasOutcome, HostedExecution, HostedObservation, HostedRecord, HostedState, LIVE,
        ProcessStamp, list_local_herdr_rows_pg, retire_pg,
    };
    use crate::services::claude::herdr_turn::{HoldRelease, input_holds, release_hold};
    use crate::services::session_host::{PaneProvider, PaneReading, herdr_endpoints};

    /// Why a retire changed nothing: the row and every hold are as they were.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) enum RetireRefusal {
        NoLocalEndpoint,
        /// No live Herdr row of this channel on this node, or more than one.
        NoRow(&'static str),
        ProviderRunning,
        /// The pane was not read as one its execution has left; it may still run something.
        Unproven(&'static str),
        /// The row changed after it was read, or the transition was refused.
        Changed(String),
    }

    /// A retired execution and what became of its input hold.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct Retired {
        pub nonce: String,
        /// This run retired the row; otherwise an earlier one did and only its hold was left.
        pub now: bool,
        pub hold: HoldRelease,
    }

    pub(super) fn run(command: HerdrCommand) -> Result<(), String> {
        let config = crate::config::load().map_err(|error| format!("load config: {error}"))?;
        super::super::direct::run_async(async move {
            let pool = crate::db::postgres::connect(&config)
                .await?
                .ok_or("postgres pool unavailable for herdr")?;
            super::install_observation_hosts(&config, &pool).await?;
            let result = match command {
                HerdrCommand::Status => status(&pool).await.map(|status| pretty(&status)),
                HerdrCommand::Retire { channel } => match retire(&pool, channel).await {
                    Ok(retired) => Ok(retired_text(channel, &retired)),
                    Err(refusal) => Err(format!("herdr retire {channel} refused: {refusal:?}")),
                },
                HerdrCommand::Terminate { .. } => Err("herdr terminate goes to the server".into()),
                HerdrCommand::ProbeUserDefault { .. } => {
                    Err("herdr probe-user-default reads no database".into())
                }
            };
            pool.close().await;
            println!("{}", result?);
            Ok(())
        })
    }

    fn pretty(value: &Value) -> String {
        serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
    }

    /// The row's retirement and the hold's removal, reported apart.
    fn retired_text(channel: u64, retired: &Retired) -> String {
        let row = if retired.now {
            "retired"
        } else {
            "was already retired"
        };
        let hold = match &retired.hold {
            HoldRelease::Released => "no input hold is left".to_string(),
            HoldRelease::NotDurable(error) => {
                format!(
                    "its input hold was removed, but the removal is not confirmed durable: {error}"
                )
            }
            HoldRelease::Kept(error) => {
                format!("its input hold remains; running retire again removes it: {error}")
            }
        };
        format!(
            "channel {channel}: execution {} {row}\nchannel {channel}: {hold}",
            retired.nonce
        )
    }

    fn local_node() -> Result<String, RetireRefusal> {
        let node = crate::config::session_hosts::local_node();
        node.filter(|_| !herdr_endpoints().is_empty())
            .ok_or(RetireRefusal::NoLocalEndpoint)
    }

    /// The pane of `record` read on its registered endpoint, off the async runtime; launch evidence
    /// is not needed to read it. `None` without a recorded pane on a registered endpoint.
    async fn read_pane(record: &HostedExecution) -> Option<PaneReading> {
        let view = herdr_endpoints().view(record)?;
        let read = tokio::task::spawn_blocking(move || view.read_execution()).await;
        Some(read.unwrap_or_else(|error| PaneReading::Unreadable(error.to_string())))
    }

    /// As `read_pane`, with whether that reading shows `record`'s execution ended.
    async fn read_end(record: &HostedExecution) -> Option<(PaneReading, bool)> {
        let view = herdr_endpoints().view(record)?;
        let record = record.clone();
        let read = tokio::task::spawn_blocking(move || {
            let reading = view.read_execution();
            let ended = view.ended(&record, &reading);
            (reading, ended)
        });
        Some(
            read.await
                .unwrap_or_else(|error| (PaneReading::Unreadable(error.to_string()), false)),
        )
    }

    fn known(row: &HostedObservation) -> Option<&HostedExecution> {
        match &row.record {
            HostedRecord::Known(record) => Some(record),
            _ => None,
        }
    }

    /// Retires `channel`'s execution only when its pane reads as gone, or as a lone shell that is
    /// its recorded root or outlived both recorded processes; then only that execution's hold goes.
    pub(crate) async fn retire(pool: &PgPool, channel: u64) -> Result<Retired, RetireRefusal> {
        let node = local_node()?;
        let rows = list_local_herdr_rows_pg(pool, &node, LIVE)
            .await
            .map_err(RetireRefusal::Changed)?;
        let channel = channel.to_string();
        let mut matching = rows
            .iter()
            .filter(|row| known(row).is_some_and(|record| record.owner.channel_id == channel));
        let row = match (matching.next(), matching.next()) {
            (Some(row), None) => row,
            (None, None) => return release_left_hold(pool, &node, &channel).await,
            _ => {
                return Err(RetireRefusal::NoRow(
                    "more than one live herdr row of the channel",
                ));
            }
        };
        let record = known(row).ok_or(RetireRefusal::NoRow("unreadable record"))?;
        let Some((reading, ended)) = read_end(record).await else {
            return Err(match record.location {
                None => RetireRefusal::Unproven("no recorded pane"),
                Some(_) => RetireRefusal::NoLocalEndpoint,
            });
        };
        retire_on_reading(pool, row, record, &reading, ended).await
    }

    /// The channel's execution an earlier retire already wrote Retired, whose hold that retire
    /// could not remove: only that hold is removed now.
    async fn release_left_hold(
        pool: &PgPool,
        node: &str,
        channel: &str,
    ) -> Result<Retired, RetireRefusal> {
        let retired = list_local_herdr_rows_pg(pool, node, &["retired"])
            .await
            .map_err(RetireRefusal::Changed)?;
        let holds = input_holds().unwrap_or_default();
        let mut left = retired.iter().filter_map(known).filter(|record| {
            record.owner.channel_id == channel
                && holds
                    .iter()
                    .any(|(held, _)| *held == record.execution_nonce)
        });
        let (Some(record), None) = (left.next(), left.next()) else {
            return Err(RetireRefusal::NoRow(
                "no single live herdr row of the channel",
            ));
        };
        Ok(Retired {
            nonce: record.execution_nonce.clone(),
            now: false,
            hold: release_hold(&record.execution_nonce),
        })
    }

    /// The retire decision on one reading and whether it shows the execution ended; the hold goes
    /// only after the row's CAS wrote Retired.
    pub(crate) async fn retire_on_reading(
        pool: &PgPool,
        row: &HostedObservation,
        record: &HostedExecution,
        reading: &PaneReading,
        ended: bool,
    ) -> Result<Retired, RetireRefusal> {
        if !ended {
            return Err(match reading {
                PaneReading::Present {
                    provider: PaneProvider::Execution(_),
                    ..
                } => RetireRefusal::ProviderRunning,
                PaneReading::Present {
                    provider: PaneProvider::Exited,
                    ..
                } if record.expected.is_none() => RetireRefusal::Unproven("no recorded root shell"),
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
                now: true,
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
            None if record.location.is_none() => "no_location",
            None => "no_local_endpoint",
            Some(PaneReading::Missing) => "missing",
            Some(PaneReading::Unreadable(_)) => "unreadable",
            Some(PaneReading::Present { provider, root }) => match provider {
                PaneProvider::Execution(_) => "provider_running",
                PaneProvider::Exited if record.expected.is_none() => "root_shell_unrecorded",
                PaneProvider::Exited if root_kept(root) => "provider_exited",
                PaneProvider::Exited => "root_replaced",
                PaneProvider::Unverified(_) => "provider_unverified",
            },
        }
    }

    /// Every local row with its pane as read now, retired rows whose hold is left, and every
    /// other hold by nonce and recorded time; no path or input text is shown.
    pub(crate) async fn status(pool: &PgPool) -> Result<Value, String> {
        let node = local_node().map_err(|error| format!("{error:?}"))?;
        let rows = list_local_herdr_rows_pg(pool, &node, LIVE).await?;
        let retired = list_local_herdr_rows_pg(pool, &node, &["retired"]).await?;
        let mut holds = input_holds()?;
        let mut take_hold = |nonce: &str| {
            let held = holds.iter().position(|(held, _)| held == nonce);
            held.map(|at| json!({"recorded_at": holds.remove(at).1}))
        };
        let mut shown = Vec::new();
        for row in rows.iter().chain(&retired) {
            let Some(record) = known(row) else {
                shown.push(json!({"session_row": row.session_id(), "record": "unreadable"}));
                continue;
            };
            let hold = take_hold(&record.execution_nonce);
            let mut execution = json!({
                "channel": record.owner.channel_id,
                "provider": record.owner.provider,
                "state": record.state,
                "nonce": record.execution_nonce,
                "input_hold": hold,
            });
            // A retired row is shown only for the hold its retire left; its pane is not read.
            if record.state == HostedState::Retired {
                if !execution["input_hold"].is_null() {
                    shown.push(execution);
                }
                continue;
            }
            let evidence = record.expected.as_ref().map_or("none", |_| "recorded");
            execution["launch_evidence"] = evidence.into();
            execution["pane"] = pane_text(read_pane(record).await.as_ref(), record).into();
            shown.push(execution);
        }
        let other: Vec<Value> = holds
            .into_iter()
            .map(|(nonce, at)| json!({"nonce": nonce, "recorded_at": at}))
            .collect();
        Ok(json!({"executions": shown, "other_input_holds": other}))
    }
}

#[cfg(all(test, unix))]
pub(crate) use node::{RetireRefusal, Retired, retire, retire_on_reading, status};

#[cfg(unix)]
#[path = "herdr_probe.rs"]
mod probe;

#[cfg(test)]
#[path = "herdr_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "herdr_terminate_tests.rs"]
mod terminate_tests;
