use sqlx::PgPool;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum SessionOwnerResolution {
    NoOwner,
    LiveLocal {
        instance_id: String,
        stale_instance_ids: Vec<String>,
    },
    LiveForeign {
        instance_id: String,
        stale_instance_ids: Vec<String>,
    },
    StaleOwners {
        instance_ids: Vec<String>,
    },
    ConflictingLiveOwners {
        instance_ids: Vec<String>,
    },
}

/// Resolve the durable tmux owner for one Discord intake identity.
///
/// Session heartbeats are deliberately not used as a lease: an idle tmux may
/// be healthy without emitting output. Foreign-owner liveness comes from the
/// existing worker-node heartbeat and provider-specific intake capability.
pub(super) async fn resolve_session_owner(
    pool: &PgPool,
    provider: &str,
    channel_id: &str,
    local_instance_id: &str,
    worker_lease_ttl_secs: u64,
) -> Result<SessionOwnerResolution, String> {
    let mut instance_ids: Vec<String> = sqlx::query_scalar(
        r#"
        SELECT DISTINCT NULLIF(BTRIM(instance_id), '') AS instance_id
          FROM sessions
         WHERE channel_id = $1
           AND LOWER(BTRIM(provider)) = LOWER(BTRIM($2))
           AND COALESCE(LOWER(BTRIM(status)), '') NOT IN ('disconnected', 'aborted')
           AND NULLIF(BTRIM(instance_id), '') IS NOT NULL
        "#,
    )
    .bind(channel_id)
    .bind(provider)
    .fetch_all(pool)
    .await
    .map_err(|error| format!("query session owner: {error}"))?;

    instance_ids.sort();
    instance_ids.dedup();
    if instance_ids.is_empty() {
        return Ok(SessionOwnerResolution::NoOwner);
    }

    let has_foreign_candidate = instance_ids
        .iter()
        .any(|instance_id| instance_id != local_instance_id);
    let worker_nodes = if has_foreign_candidate {
        crate::services::cluster::node_registry::list_worker_nodes(pool, worker_lease_ttl_secs)
            .await
            .map_err(|error| format!("classify session owner worker: {error}"))?
    } else {
        Vec::new()
    };

    let mut live_instance_ids = Vec::new();
    let mut stale_instance_ids = Vec::new();
    for instance_id in &instance_ids {
        let is_live = if instance_id == local_instance_id {
            // The gateway process is itself an executable owner even when its
            // worker-node advertisement has not landed yet.
            true
        } else {
            worker_nodes.iter().any(|node| {
                node.get("instance_id").and_then(serde_json::Value::as_str)
                    == Some(instance_id.as_str())
                    && node
                        .get("status")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|status| status.eq_ignore_ascii_case("online"))
                    && crate::services::cluster::node_registry::node_supports_intake_provider(
                        node, provider,
                    )
            })
        };
        if is_live {
            live_instance_ids.push(instance_id.clone());
        } else {
            stale_instance_ids.push(instance_id.clone());
        }
    }

    match live_instance_ids.as_slice() {
        [] => Ok(SessionOwnerResolution::StaleOwners {
            instance_ids: stale_instance_ids,
        }),
        [instance_id] if instance_id == local_instance_id => {
            Ok(SessionOwnerResolution::LiveLocal {
                instance_id: instance_id.clone(),
                stale_instance_ids,
            })
        }
        [instance_id] => Ok(SessionOwnerResolution::LiveForeign {
            instance_id: instance_id.clone(),
            stale_instance_ids,
        }),
        _ => Ok(SessionOwnerResolution::ConflictingLiveOwners {
            instance_ids: live_instance_ids,
        }),
    }
}
