//! A shared PG observation is a plan input, never execution permission.
use super::{OwnerIdentity, OwnerRecordSnapshot, custody, fallback};
use crate::services::cluster::{execution_requirements::ExecutionRequirements, readiness};
use fallback::{Availability, Home, Target};
use serde_json::{Value, json};
use sqlx::{Postgres, Transaction};

#[derive(Clone)]
pub(crate) struct Context {
    pub policy_channel: String,
    pub pair: [String; 2],
    pub gateway: Option<String>,
    pub auth_profile: String,
    pub lease_seconds: i64,
}

pub(crate) struct Shared {
    pub context: Context,
    pub home: Option<String>,
    pub conflict: bool,
    pub proof: Value,
    pub availability: [Availability; 2],
    pub owner: Option<OwnerRecordSnapshot>,
    pub raw_revision: i64,
}

impl Shared {
    pub(crate) fn target(&self) -> Target<'_> {
        let home = if self.conflict {
            Home::Conflict
        } else {
            self.home.as_deref().map_or(Home::Unknown, Home::Known)
        };
        fallback::choose_target(&fallback::Snapshot {
            home,
            pair: [&self.context.pair[0], &self.context.pair[1]],
            availability: self.availability,
            gateway: self.context.gateway.as_deref(),
        })
    }
}

fn override_home(value: &Value) -> Option<String> {
    (value["mode"] == "override")
        .then(|| value["home_instance_id"].as_str().map(str::to_owned))
        .flatten()
}

/// Each node publishes its own control/REST providers; the gateway never vouches for another Mac.
fn published(node: &Value, kind: &str, provider: &str) -> Availability {
    match node["capabilities"]["routing_owner"][kind].as_array() {
        None => Availability::Unknown,
        Some(list) if list.iter().any(|p| p.as_str() == Some(provider)) => Availability::Available,
        Some(_) => Availability::Unavailable,
    }
}

/// Control/REST observations come from each node's fresh heartbeat, not gateway ownership.
pub(crate) async fn read_on_tx(
    tx: &mut Transaction<'_, Postgres>,
    id: &OwnerIdentity,
    context: &Context,
) -> Result<Shared, sqlx::Error> {
    custody::lock(tx, id).await?;
    let (now, data): (i64, Value) = sqlx::query_as(
        "SELECT (EXTRACT(EPOCH FROM NOW())*1000)::BIGINT, jsonb_build_object(
         'raw',(SELECT to_jsonb(h)-'updated_at' FROM channel_home_preferences h WHERE provider=$1 AND raw_channel_id=$2),
         'parent',(SELECT to_jsonb(h)-'updated_at' FROM channel_home_preferences h WHERE provider=$1 AND raw_channel_id=$3 AND $2<>$3),
         'agents',(SELECT COALESCE(jsonb_agg(jsonb_build_object('id',id,'home',default_execution_node_id,'requirements',execution_requirements) ORDER BY id),'[]'::jsonb) FROM agents WHERE discord_channel_id=$3 OR discord_channel_alt=$3 OR discord_channel_cc=$3 OR discord_channel_cdx=$3),
         'nodes',(SELECT COALESCE(jsonb_agg(jsonb_build_object('instance_id',instance_id,'status',CASE WHEN last_heartbeat_at IS NULL THEN NULL WHEN last_heartbeat_at<NOW()-make_interval(secs=>$4::DOUBLE PRECISION) THEN 'offline' ELSE status END,'capabilities',capabilities)),'[]'::jsonb) FROM worker_nodes WHERE instance_id=ANY($5)))",
    )
    .bind(id.provider()).bind(id.raw_channel_id()).bind(&context.policy_channel)
    .bind(context.lease_seconds.max(1) as f64).bind(context.pair.to_vec())
    .fetch_one(&mut **tx).await?;
    let agents = data["agents"].as_array().cloned().unwrap_or_default();
    let defaults: std::collections::BTreeSet<Option<String>> = agents
        .iter()
        .map(|agent| agent["home"].as_str().map(str::to_owned))
        .collect();
    let explicit = override_home(&data["raw"]).or_else(|| override_home(&data["parent"]));
    let conflict = explicit.is_none() && defaults.len() > 1;
    let home = explicit.or_else(|| {
        (!conflict)
            .then(|| defaults.into_iter().next().flatten())
            .flatten()
    });
    let requirements: Result<Vec<_>, _> = agents
        .iter()
        .map(|agent| ExecutionRequirements::parse(agent["requirements"].clone()))
        .collect();
    let mut availability = [Availability::Unknown; 2];
    for (index, instance) in context.pair.iter().enumerate() {
        let node = data["nodes"]
            .as_array()
            .and_then(|nodes| nodes.iter().find(|n| n["instance_id"] == *instance));
        let Some(node) = node else { continue };
        if node["status"] == "offline" {
            availability[index] = Availability::Unavailable;
            continue;
        }
        if node["status"] != "online"
            || readiness::evidence(node, now).is_err()
            || requirements.is_err()
        {
            continue;
        }
        if node["capabilities"]["execution_readiness"]["providers"]
            .get(id.provider())
            .is_none()
        {
            continue;
        }
        let worker = crate::services::cluster::node_registry::node_supports_intake_request(
            node,
            id.provider(),
            true,
        );
        let ready = readiness::evaluate(node, id.provider(), &context.auth_profile, now).eligible
            && requirements
                .as_ref()
                .is_ok_and(|items| items.iter().all(|r| r.explain(node, now).is_empty()));
        let owner = [
            published(node, "control", id.provider()),
            published(node, "rest", id.provider()),
        ];
        availability[index] = if !worker || !ready {
            Availability::Unavailable
        } else if owner.contains(&Availability::Unknown) {
            Availability::Unknown
        } else if owner.contains(&Availability::Unavailable) {
            Availability::Unavailable
        } else {
            Availability::Available
        };
    }
    #[cfg(test)]
    if super::cleanup::tests::mutant("fallback-unknown") {
        for available in &mut availability {
            if *available == Availability::Unknown {
                *available = Availability::Unavailable;
            }
        }
    }
    let proof = json!({"raw":data["raw"],"parent":data["parent"],"agents":data["agents"],
        "policy_channel":context.policy_channel,"auth_profile":context.auth_profile});
    Ok(Shared {
        raw_revision: data["raw"]["revision"].as_i64().unwrap_or(0),
        context: context.clone(),
        home,
        conflict,
        proof,
        availability,
        owner: super::read_latest_owner_in_tx(tx, id).await?,
    })
}

#[cfg(test)]
#[path = "snapshot_tests.rs"]
pub(crate) mod tests;
