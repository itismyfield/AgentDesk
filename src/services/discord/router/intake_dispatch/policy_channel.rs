//! Resolve policy inheritance without changing the channel used for ownership/delivery.
use super::*;
use crate::services::cluster::{agent_execution_node, execution_capacity};

pub(super) async fn resolve(
    deps: &IntakeDeps<'_>,
    pool: &sqlx::PgPool,
    channel: serenity::ChannelId,
    is_dm: bool,
) -> Result<String, String> {
    let id = channel.get().to_string();
    if is_dm
        || agent_execution_node::channel_is_bound(pool, &id)
            .await
            .map_err(|e| e.to_string())?
    {
        return Ok(id);
    }
    // Preserve legacy unbound-channel behavior when there is no placement policy to inherit.
    if !execution_capacity::automatic_enabled()
        && !agent_execution_node::has_channel_policy(pool)
            .await
            .map_err(|e| e.to_string())?
    {
        return Ok(id);
    }
    let metadata = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        channel.to_channel(deps.http),
    )
    .await
    .map_err(|_| "Discord channel policy lookup timed out".to_string())?
    .map_err(|e| format!("Discord channel policy lookup failed: {e}"))?;
    let serenity::Channel::Guild(guild) = metadata else {
        return Ok(id);
    };
    if !matches!(
        guild.kind,
        serenity::ChannelType::PublicThread
            | serenity::ChannelType::PrivateThread
            | serenity::ChannelType::NewsThread
    ) {
        return Ok(id);
    }
    let parent = guild
        .parent_id
        .ok_or("Discord thread has no parent channel")?;
    if !crate::services::discord::role_map::thread_inheritance_enabled(parent, None) {
        return Ok(id);
    }
    Ok(parent.get().to_string())
}
