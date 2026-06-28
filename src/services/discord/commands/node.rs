use std::sync::Arc;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use poise::CreateReply;
use poise::serenity_prelude as serenity;

use crate::services::cluster::intake_router_hook::{
    EffectiveIntakeRoutingConfig, effective_intake_routing_config,
};
use crate::services::cluster::node_registry::node_supports_intake_provider;
use crate::services::provider::ProviderKind;

use super::super::settings::save_bot_settings;
use super::super::{Context, Data, Error, SharedData, check_auth};

const NODE_PICKER_CUSTOM_ID: &str = "agentdesk:node-picker";
const NODE_PICKER_RESET_VALUE: &str = "__agentdesk_node_default__";
const NODE_PICKER_TTL: Duration = Duration::from_secs(30 * 60);
const NODE_PICKER_MAX_OPTIONS: usize = 25;

#[derive(Clone)]
struct NodePickerPending {
    owner: serenity::UserId,
    target_channel_id: serenity::ChannelId,
    updated_at: Instant,
}

#[derive(Clone, Debug)]
struct NodeChoice {
    instance_id: String,
    hostname: Option<String>,
    status: String,
    labels: Vec<String>,
}

static NODE_PICKER_PENDING: LazyLock<dashmap::DashMap<serenity::MessageId, NodePickerPending>> =
    LazyLock::new(dashmap::DashMap::new);

fn prune_node_picker_pending() {
    let now = Instant::now();
    let expired: Vec<_> = NODE_PICKER_PENDING
        .iter()
        .filter_map(|entry| {
            (now.duration_since(entry.updated_at) > NODE_PICKER_TTL).then_some(*entry.key())
        })
        .collect();
    for message_id in expired {
        NODE_PICKER_PENDING.remove(&message_id);
    }
}

fn intake_routing_unavailable_message(effective: &EffectiveIntakeRoutingConfig) -> String {
    format!(
        "`/node`는 현재 사용할 수 없습니다. 현재 intake routing mode는 `{}` (source: `{}`)입니다. \
         `cluster.intake_routing.enabled=true` 및 `mode=enforce`로 설정하거나 \
         긴급 시 `ADK_INTAKE_ROUTING_MODE=enforce` override로 실행해야 노드 선택을 저장합니다.",
        effective.mode.as_str(),
        effective.source.as_str()
    )
}

fn intake_routing_enforced(effective: &EffectiveIntakeRoutingConfig) -> bool {
    effective.mode_is_enforce()
}

pub(in crate::services::discord) fn channel_node_override(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
) -> Option<String> {
    shared
        .overrides
        .node_overrides
        .get(&channel_id)
        .map(|value| value.clone())
}

async fn update_channel_node_override(
    shared: &Arc<SharedData>,
    token: &str,
    channel_id: serenity::ChannelId,
    next_instance_id: Option<String>,
) -> bool {
    let current = channel_node_override(shared, channel_id);
    let next_instance_id = next_instance_id
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    if current.as_deref() == next_instance_id.as_deref() {
        return false;
    }

    let channel_key = channel_id.get().to_string();
    let mut settings = shared.settings.write().await;
    match next_instance_id {
        Some(instance_id) => {
            shared
                .overrides
                .node_overrides
                .insert(channel_id, instance_id.clone());
            settings
                .channel_node_overrides
                .insert(channel_key, instance_id);
        }
        None => {
            shared.overrides.node_overrides.remove(&channel_id);
            settings.channel_node_overrides.remove(&channel_key);
        }
    }
    save_bot_settings(token, &settings);
    true
}

pub(in crate::services::discord) fn is_node_picker_custom_id(custom_id: &str) -> bool {
    custom_id == NODE_PICKER_CUSTOM_ID
}

fn truncate_discord_field(value: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for ch in value.chars().take(max_chars) {
        out.push(ch);
    }
    if value.chars().count() > max_chars {
        out.push('…');
    }
    out
}

fn node_labels(node: &serde_json::Value) -> Vec<String> {
    node.get("labels")
        .and_then(|value| value.as_array())
        .map(|labels| {
            labels
                .iter()
                .filter_map(|value| value.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn node_choice_from_json(node: &serde_json::Value) -> Option<NodeChoice> {
    let instance_id = node
        .get("instance_id")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())?
        .to_string();
    if instance_id.len() > 100 {
        return None;
    }
    Some(NodeChoice {
        instance_id,
        hostname: node
            .get("hostname")
            .and_then(|value| value.as_str())
            .map(str::to_string),
        status: node
            .get("status")
            .and_then(|value| value.as_str())
            .unwrap_or("unknown")
            .to_string(),
        labels: node_labels(node),
    })
}

async fn list_online_node_choices(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
) -> Result<Vec<NodeChoice>, String> {
    let Some(pool) = shared.pg_pool.as_ref() else {
        return Err(
            "Postgres pool is unavailable; cluster node registry cannot be read.".to_string(),
        );
    };
    let lease_ttl_secs = crate::config::load_graceful().cluster.lease_ttl_secs.max(1);
    let mut nodes =
        crate::services::cluster::node_registry::list_worker_nodes(pool, lease_ttl_secs).await?;
    nodes.sort_by(|left, right| {
        let left_id = left
            .get("instance_id")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        let right_id = right
            .get("instance_id")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        left_id.cmp(right_id)
    });
    Ok(nodes
        .iter()
        .filter(|node| node_supports_intake_provider(node, provider.as_str()))
        .filter_map(node_choice_from_json)
        .filter(|node| node.status.eq_ignore_ascii_case("online"))
        .take(NODE_PICKER_MAX_OPTIONS - 1)
        .collect())
}

fn node_option(
    node: &NodeChoice,
    selected_instance_id: Option<&str>,
) -> serenity::CreateSelectMenuOption {
    let label = match node.hostname.as_deref() {
        Some(hostname) if !hostname.trim().is_empty() => {
            format!("{} ({})", hostname.trim(), node.instance_id)
        }
        _ => node.instance_id.clone(),
    };
    let labels = if node.labels.is_empty() {
        "labels: none".to_string()
    } else {
        format!("labels: {}", node.labels.join(", "))
    };
    let option = serenity::CreateSelectMenuOption::new(
        truncate_discord_field(&label, 100),
        node.instance_id.clone(),
    )
    .description(truncate_discord_field(&labels, 100));
    if selected_instance_id == Some(node.instance_id.as_str()) {
        option.default_selection(true)
    } else {
        option
    }
}

fn build_node_picker_components(
    nodes: &[NodeChoice],
    selected_instance_id: Option<&str>,
) -> Vec<serenity::CreateActionRow> {
    let mut options = vec![
        serenity::CreateSelectMenuOption::new("기본 라우팅", NODE_PICKER_RESET_VALUE)
            .description("채널/에이전트 설정과 로컬 fallback을 사용합니다.")
            .default_selection(selected_instance_id.is_none()),
    ];
    options.extend(
        nodes
            .iter()
            .map(|node| node_option(node, selected_instance_id)),
    );
    let menu = serenity::CreateSelectMenu::new(
        NODE_PICKER_CUSTOM_ID,
        serenity::CreateSelectMenuKind::String { options },
    )
    .placeholder("세션을 시작할 노드 선택")
    .min_values(1)
    .max_values(1);
    vec![serenity::CreateActionRow::SelectMenu(menu)]
}

fn selected_string_value(component: &serenity::ComponentInteraction) -> Option<String> {
    match &component.data.kind {
        serenity::ComponentInteractionDataKind::StringSelect { values } => values.first().cloned(),
        _ => None,
    }
}

async fn ephemeral_reply(
    ctx: &serenity::Context,
    component: &serenity::ComponentInteraction,
    message: impl Into<String>,
) -> Result<(), Error> {
    component
        .create_response(
            ctx,
            serenity::CreateInteractionResponse::Message(
                serenity::CreateInteractionResponseMessage::new()
                    .content(message)
                    .ephemeral(true),
            ),
        )
        .await?;
    Ok(())
}

fn node_summary(nodes: &[NodeChoice], selected_instance_id: Option<&str>) -> String {
    let current = selected_instance_id.unwrap_or("기본 라우팅");
    let count = nodes.len();
    format!(
        "현재 선택: `{current}`\n사용 가능한 intake 노드 {count}개를 찾았습니다. 선택한 노드는 다음 일반 메시지부터 적용됩니다."
    )
}

/// /node — Pick the cluster node that should run this channel's session
#[poise::command(slash_command, rename = "node")]
pub(in crate::services::discord) async fn cmd_node(ctx: Context<'_>) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!("  [{ts}] ◀ [{user_name}] /node");

    let intake_routing = effective_intake_routing_config();
    if !intake_routing_enforced(&intake_routing) {
        ctx.say(intake_routing_unavailable_message(&intake_routing))
            .await?;
        return Ok(());
    }

    let channel_id = ctx.channel_id();
    let nodes = match list_online_node_choices(&ctx.data().shared, &ctx.data().provider).await {
        Ok(nodes) => nodes,
        Err(error) => {
            ctx.say(format!("클러스터 노드 목록을 읽을 수 없습니다: {error}"))
                .await?;
            return Ok(());
        }
    };
    if nodes.is_empty() {
        ctx.say("현재 provider의 intake worker를 광고하는 온라인 cluster node가 없습니다.")
            .await?;
        return Ok(());
    }

    let selected = channel_node_override(&ctx.data().shared, channel_id);
    let components = build_node_picker_components(&nodes, selected.as_deref());
    let posted = ctx
        .send(
            CreateReply::default()
                .content(node_summary(&nodes, selected.as_deref()))
                .components(components),
        )
        .await?
        .into_message()
        .await?;
    prune_node_picker_pending();
    NODE_PICKER_PENDING.insert(
        posted.id,
        NodePickerPending {
            owner: user_id,
            target_channel_id: channel_id,
            updated_at: Instant::now(),
        },
    );
    Ok(())
}

pub(in crate::services::discord) async fn handle_node_picker_interaction(
    ctx: &serenity::Context,
    component: &serenity::ComponentInteraction,
    data: &Data,
) -> Result<(), Error> {
    let user_id = component.user.id;
    let user_name = &component.user.name;
    if !check_auth(user_id, user_name, &data.shared, &data.token).await {
        return ephemeral_reply(ctx, component, "Not authorized for this bot.").await;
    }

    let message_id = component.message.id;
    let Some(pending) = NODE_PICKER_PENDING
        .get(&message_id)
        .map(|entry| entry.clone())
    else {
        return ephemeral_reply(
            ctx,
            component,
            "이 node picker가 만료됐습니다. `/node`를 다시 실행하세요.",
        )
        .await;
    };
    if Instant::now().duration_since(pending.updated_at) > NODE_PICKER_TTL {
        NODE_PICKER_PENDING.remove(&message_id);
        return ephemeral_reply(
            ctx,
            component,
            "이 node picker가 만료됐습니다. `/node`를 다시 실행하세요.",
        )
        .await;
    }
    if pending.owner != user_id {
        return ephemeral_reply(ctx, component, "패널을 연 사용자만 조작할 수 있습니다.").await;
    }

    let intake_routing = effective_intake_routing_config();
    if !intake_routing_enforced(&intake_routing) {
        NODE_PICKER_PENDING.remove(&message_id);
        component
            .create_response(
                ctx,
                serenity::CreateInteractionResponse::UpdateMessage(
                    serenity::CreateInteractionResponseMessage::new()
                        .content(intake_routing_unavailable_message(&intake_routing))
                        .components(Vec::new()),
                ),
            )
            .await?;
        return Ok(());
    }

    let Some(selected) = selected_string_value(component) else {
        component
            .create_response(ctx, serenity::CreateInteractionResponse::Acknowledge)
            .await?;
        return Ok(());
    };

    let selected_instance = if selected == NODE_PICKER_RESET_VALUE {
        None
    } else {
        let nodes = match list_online_node_choices(&data.shared, &data.provider).await {
            Ok(nodes) => nodes,
            Err(error) => {
                return ephemeral_reply(
                    ctx,
                    component,
                    format!("클러스터 노드 목록을 다시 확인할 수 없습니다: {error}"),
                )
                .await;
            }
        };
        if !nodes.iter().any(|node| node.instance_id == selected) {
            return ephemeral_reply(
                ctx,
                component,
                "선택한 노드가 현재 provider의 intake worker를 광고하지 않습니다. `/node`를 다시 실행하세요.",
            )
            .await;
        }
        Some(selected)
    };

    let changed = update_channel_node_override(
        &data.shared,
        &data.token,
        pending.target_channel_id,
        selected_instance.clone(),
    )
    .await;
    let target = selected_instance.unwrap_or_else(|| "기본 라우팅".to_string());
    let change_note = if changed {
        "저장했습니다"
    } else {
        "이미 같은 설정입니다"
    };
    let content =
        format!("`/node` {change_note}: 이 채널의 다음 세션/turn 대상은 `{target}` 입니다.");

    component
        .create_response(
            ctx,
            serenity::CreateInteractionResponse::UpdateMessage(
                serenity::CreateInteractionResponseMessage::new()
                    .content(content)
                    .components(Vec::new()),
            ),
        )
        .await?;
    NODE_PICKER_PENDING.remove(&message_id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ClusterIntakeRoutingMode;
    use crate::services::cluster::intake_router_hook::{
        IntakeRoutingMode, IntakeRoutingModeSource,
    };

    #[test]
    fn node_unavailable_message_reports_effective_mode_and_source() {
        let effective = EffectiveIntakeRoutingConfig {
            mode: IntakeRoutingMode::Observe,
            source: IntakeRoutingModeSource::EnvOverride,
            yaml_enabled: true,
            yaml_mode: ClusterIntakeRoutingMode::Enforce,
            env_override: Some("observe"),
            warnings: Vec::new(),
            forward_pre_claim_timeout_secs: 12,
            stale_claim_recovery_secs: 60,
        };

        let message = intake_routing_unavailable_message(&effective);
        assert!(message.contains("mode는 `observe`"));
        assert!(message.contains("source: `env_override`"));
        assert!(message.contains("cluster.intake_routing.enabled=true"));
        assert!(message.contains("ADK_INTAKE_ROUTING_MODE=enforce"));
        assert!(!intake_routing_enforced(&effective));
    }
}
