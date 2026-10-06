//! `agentdesk channel-home`: delegated channel homes. `status` only reads; delegate, reclaim and
//! force each write one conditional row change while `runtime.channel_home_delegation_enabled` is on.

use std::future::Future;

use clap::{Args, Subcommand};
use serde_json::{Value, json};
use sqlx::PgPool;

use crate::config::Config;
use crate::db::o_channel_homes::{
    self, ChannelHome, ForceOutcome, ForceWindow, HomeProvider, HomeState, HomeWrite,
};

#[derive(Args)]
#[command(about = "Delegated channel homes: status, and the switched delegate, reclaim and force")]
pub(crate) struct ChannelHomeArgs {
    #[command(subcommand)]
    command: ChannelHomeCommand,
}

#[derive(Subcommand, Debug)]
pub(crate) enum ChannelHomeCommand {
    /// Each home row and its open intake by routed epoch; changes nothing
    Status,
    /// Start releasing a gateway-owned channel from this node to `--to`
    Delegate {
        channel: u64,
        /// claude or codex
        #[arg(long)]
        provider: String,
        #[arg(long)]
        to: String,
    },
    /// Start draining a worker-owned channel back to this node
    Reclaim { channel: u64 },
    /// Orphan a holder whose lease has been silent past F; nothing adopts the channel after
    Force { channel: u64 },
}

impl ChannelHomeCommand {
    fn mutates(&self) -> bool {
        !matches!(self, Self::Status)
    }
}

pub(crate) fn run(args: ChannelHomeArgs) -> Result<(), String> {
    let config = crate::config::load().map_err(|error| format!("load config: {error}"))?;
    super::direct::run_async(async move {
        let connect = || async {
            crate::db::postgres::connect(&config)
                .await?
                .ok_or_else(|| "postgres pool unavailable for channel-home".to_string())
        };
        println!("{}", execute(&config, args.command, connect).await?);
        Ok(())
    })
}

/// Refuses a mutation while the switch is off, and checks its inputs, before connecting.
pub(crate) async fn execute<C, F>(
    config: &Config,
    command: ChannelHomeCommand,
    connect: C,
) -> Result<String, String>
where
    C: FnOnce() -> F,
    F: Future<Output = Result<PgPool, String>>,
{
    let switched_on = config.runtime.channel_home_delegation_enabled == Some(true);
    if command.mutates() && !switched_on {
        return Err("channel-home refused: runtime.channel_home_delegation_enabled is off".into());
    }
    let local = config.cluster.instance_id.as_deref().map(str::trim);
    let local = local.filter(|node| !node.is_empty());
    let unset = || "channel-home refused: cluster.instance_id is not set".to_string();
    match &command {
        ChannelHomeCommand::Delegate { provider, to, .. } => {
            let local = local.ok_or_else(unset)?;
            if HomeProvider::parse(provider).is_none() {
                return Err(format!(
                    "channel-home refused: provider {provider:?} is not claude or codex"
                ));
            }
            if to.trim().is_empty() || to.trim() == local {
                return Err(format!(
                    "channel-home refused: target {to:?} is not another node"
                ));
            }
        }
        ChannelHomeCommand::Reclaim { .. } => {
            local.ok_or_else(unset)?;
        }
        ChannelHomeCommand::Status | ChannelHomeCommand::Force { .. } => {}
    }
    let local = local.unwrap_or_default();
    let pool = connect().await?;
    let result = match command {
        ChannelHomeCommand::Status => status(&pool).await,
        ChannelHomeCommand::Delegate {
            channel,
            provider,
            to,
        } => delegate(&pool, channel, &provider, local, to.trim()).await,
        ChannelHomeCommand::Reclaim { channel } => reclaim(&pool, channel, local).await,
        ChannelHomeCommand::Force { channel } => force(&pool, channel).await,
    };
    pool.close().await;
    result
}

fn row_view(home: &ChannelHome) -> Value {
    json!({"channel": home.channel_id, "provider": home.provider, "state": home.state.as_str(),
        "holder": home.holder, "target": home.target, "epoch": home.epoch,
        "renewed_at": home.renewed_at, "updated_at": home.updated_at})
}

/// Rows and open intake counts only; no intake text or path is printed.
async fn status(pool: &PgPool) -> Result<String, String> {
    let homes = o_channel_homes::list_homes(pool)
        .await
        .map_err(|e| e.to_string())?;
    let mut listed = Vec::new();
    for home in &homes {
        let open = o_channel_homes::open_intake_by_epoch(pool, &home.channel_id, home.epoch)
            .await
            .map_err(|e| e.to_string())?;
        let mut view = row_view(home);
        view["open_intake"] = json!({"current_epoch": open.current,
            "other_epoch": open.other_epoch, "unrouted": open.unrouted});
        listed.push(view);
    }
    let body = json!({"homes": listed});
    Ok(serde_json::to_string_pretty(&body).unwrap_or_else(|_| body.to_string()))
}

async fn current(pool: &PgPool, channel: &str) -> Result<ChannelHome, String> {
    let home = o_channel_homes::read_home(pool, channel)
        .await
        .map_err(|e| e.to_string())?;
    home.ok_or(format!(
        "channel-home refused: channel {channel} has no home row"
    ))
}

fn written(command: &str, write: HomeWrite<ChannelHome>) -> Result<ChannelHome, String> {
    match write {
        HomeWrite::Applied(home) => Ok(home),
        HomeWrite::Stale => Err(format!("channel-home {command} refused: the row changed")),
    }
}

/// The written row with the node that ran the command and the holder and target it named; both
/// come from this node's `cluster.instance_id`, never from the gateway preference.
fn planned(node: &str, holder: Option<&str>, target: &str, home: &ChannelHome) -> String {
    let planned = json!({"holder": holder, "target": target});
    json!({"run_on": node, "planned": planned, "row": row_view(home)}).to_string()
}

async fn delegate(
    pool: &PgPool,
    channel: u64,
    provider: &str,
    gateway: &str,
    to: &str,
) -> Result<String, String> {
    let channel = channel.to_string();
    let write = o_channel_homes::delegate(pool, &channel, provider, gateway, to).await;
    let home = written("delegate", write.map_err(|e| e.to_string())?)?;
    Ok(planned(gateway, Some(gateway), to, &home))
}

async fn reclaim(pool: &PgPool, channel: u64, gateway: &str) -> Result<String, String> {
    let channel = channel.to_string();
    let home = current(pool, &channel).await?;
    if home.state != HomeState::Worker {
        return Err(format!(
            "channel-home reclaim refused: state is {}",
            home.state.as_str()
        ));
    }
    let write = o_channel_homes::begin_reclaim(pool, &channel, home.epoch, gateway).await;
    let reclaiming = written("reclaim", write.map_err(|e| e.to_string())?)?;
    Ok(planned(
        gateway,
        home.holder.as_deref(),
        gateway,
        &reclaiming,
    ))
}

async fn force(pool: &PgPool, channel: u64) -> Result<String, String> {
    let channel = channel.to_string();
    let home = current(pool, &channel).await?;
    let detail = "operator force";
    let forced =
        o_channel_homes::force_orphan(pool, &channel, home.epoch, ForceWindow::MIN, detail);
    match forced.await.map_err(|e| e.to_string())? {
        ForceOutcome::Orphaned(home) => Ok(row_view(&home).to_string()),
        ForceOutcome::Fresh => Err("channel-home force refused: the lease renewed within F".into()),
        ForceOutcome::Stale => Err("channel-home force refused: no holder at that epoch".into()),
    }
}

#[cfg(test)]
#[path = "channel_home_tests.rs"]
mod tests;
