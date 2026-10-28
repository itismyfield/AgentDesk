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

use super::super::{Context, Data, Error, SharedData, check_auth};
use crate::db::channel_home_preferences::{self as homes, WriteOutcome};
use crate::services::cluster::intake_router_hook::owner_record::OwnerIdentity;
use uuid::Uuid;

const NODE_PICKER_CUSTOM_ID: &str = "agentdesk:node-picker";
const NODE_PICKER_RESET_VALUE: &str = "__agentdesk_node_default__";
const NODE_PICKER_TTL: Duration = Duration::from_secs(30 * 60);
const NODE_PICKER_MAX_OPTIONS: usize = 25;

#[derive(Clone)]
struct NodePickerPending {
    owner: serenity::UserId,
    target_channel_id: serenity::ChannelId,
    updated_at: Instant,
    revision: i64,
    operation: Uuid,
    provider: String,
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

pub(in crate::services::discord) async fn channel_node_override(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
) -> Result<Option<String>, String> {
    let pool = shared.pg_pool.as_ref().ok_or("shared home requires PG")?;
    let identity = OwnerIdentity::new(shared.provider.as_str(), &channel_id.get().to_string());
    homes::read(pool, &identity)
        .await
        .map(|row| row.and_then(|preference| preference.home_instance_id))
        .map_err(|error| format!("shared home read failed: {error}"))
}

async fn update_channel_node_override(
    pool: &sqlx::PgPool,
    identity: &OwnerIdentity,
    revision: i64,
    operation: Uuid,
    next: Option<&str>,
) -> Result<WriteOutcome, Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(identity.advisory_key())
        .execute(&mut *tx)
        .await?;
    let outcome = homes::write_on_tx(&mut tx, identity, revision, operation, next).await?;
    tx.commit().await?;
    let read_back = homes::read(pool, identity).await?;
    if matches!(&outcome, WriteOutcome::Applied(row) | WriteOutcome::Reused(row)
        if read_back.as_ref() == Some(row))
    {
        Ok(outcome)
    } else {
        Ok(WriteOutcome::Conflict(read_back))
    }
}

fn home_write_note(outcome: &WriteOutcome) -> &'static str {
    match outcome {
        WriteOutcome::Applied(_) | WriteOutcome::Reused(_) => "저장했습니다",
        WriteOutcome::Conflict(_) => {
            "저장 여부 미확정입니다. `/node`로 현재 설정을 다시 확인하세요"
        }
    }
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
    let lease_ttl_secs = crate::config::runtime_config()
        .cluster
        .lease_ttl_secs
        .max(1);
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
    .description(truncate_discord_field(
        &format!("{} · {labels}", node.status),
        100,
    ));
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

    log_command_received!(ctx.channel_id().get(), user_name, "/node");

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
        ctx.say("현재 provider의 intake worker를 광고하는 등록된 cluster node가 없습니다.")
            .await?;
        return Ok(());
    }

    let pool = ctx
        .data()
        .shared
        .pg_pool
        .as_ref()
        .ok_or("shared home requires PG")?;
    let identity = OwnerIdentity::new(ctx.data().provider.as_str(), &channel_id.get().to_string());
    let preference = homes::read(pool, &identity).await?;
    let revision = preference.as_ref().map_or(0, |row| row.revision);
    let selected = preference.and_then(|row| row.home_instance_id);
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
            revision,
            operation: Uuid::new_v4(),
            provider: ctx.data().provider.as_str().to_owned(),
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

    let pool = data
        .shared
        .pg_pool
        .as_ref()
        .ok_or("shared home requires PG")?;
    let identity = OwnerIdentity::new(
        &pending.provider,
        &pending.target_channel_id.get().to_string(),
    );
    let outcome = match update_channel_node_override(
        pool,
        &identity,
        pending.revision,
        pending.operation,
        selected_instance.as_deref(),
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(error) => {
            return ephemeral_reply(
                ctx,
                component,
                format!("저장 여부 미확정입니다: {error}. `/node`로 다시 확인하세요."),
            )
            .await;
        }
    };
    let target = selected_instance.unwrap_or_else(|| "기본 라우팅".to_string());
    let content = if let WriteOutcome::Conflict(current) = &outcome {
        let home = current
            .as_ref()
            .and_then(|row| row.home_instance_id.as_deref())
            .unwrap_or("기본 라우팅");
        let revision = current.as_ref().map_or(0, |row| row.revision);
        format!(
            "{} (현재 `{home}`, revision {revision})",
            home_write_note(&outcome)
        )
    } else {
        format!(
            "`/node` {}: 새 세션의 대상은 `{target}` 입니다. 기존 live 세션은 현재 owner 노드에 계속 고정됩니다.",
            home_write_note(&outcome)
        )
    };

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
    use crate::services::cluster::intake_router_hook::IntakeRoutingMode;
    use crate::services::cluster::intake_routing_config::IntakeRoutingModeSource;

    #[test]
    fn node_unavailable_message_reports_effective_mode_and_source() {
        let effective = EffectiveIntakeRoutingConfig {
            mode: IntakeRoutingMode::Observe,
            source: IntakeRoutingModeSource::EnvOverride,
            yaml_enabled: true,
            yaml_mode: ClusterIntakeRoutingMode::Enforce,
            env_override: Some("observe"),
            warnings: Vec::new(),
            owner_authority_channel_ids: Vec::new(),
            owner_authority_config_known: true,
            forward_pre_claim_timeout_secs: 12,
            stale_claim_recovery_secs: 60,
            max_attempts_per_message: 5,
            retry_authorization_secs: 300,
        };

        let message = intake_routing_unavailable_message(&effective);
        assert!(message.contains("mode는 `observe`"));
        assert!(message.contains("source: `env_override`"));
        assert!(message.contains("cluster.intake_routing.enabled=true"));
        assert!(message.contains("ADK_INTAKE_ROUTING_MODE=enforce"));
        assert!(!intake_routing_enforced(&effective));
    }
}

#[cfg(test)]
mod home_pg_tests;

#[cfg(test)]
#[path = "node_snapshot_tests.rs"]
mod snapshot_tests;
