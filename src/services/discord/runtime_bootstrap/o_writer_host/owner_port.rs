//! REST and first-init facts derive from the pinned channel execution permit.
use super::*;
use crate::services::cluster::intake_router_hook::owner_record::output::Scope;

pub(super) async fn open(
    host: &GatewayHost,
    channel: u64,
    permit: &OwnerPermit,
) -> Result<Arc<GatewayPort>, String> {
    let prepare = async {
        if permit.identity.provider() != host.shared.provider.as_str()
            || permit.identity.raw_channel_id() != channel.to_string()
            || permit.executor != local_id(host.configured_id.as_deref(), host.self_id_wait).await?
        {
            return Err("channel execution permit does not identify this runtime".into());
        }
        let runtime = crate::services::discord::health::resolve_intake_worker_runtime(
            &host.shared,
            ChannelId::new(channel),
        )
        .await?;
        if runtime.shared.provider != host.shared.provider {
            return Err("channel REST runtime provider changed".into());
        }
        let http = runtime.http;
        #[cfg(test)]
        let http = host.rest.clone().unwrap_or(http);
        let user = http
            .get_current_user()
            .await
            .map_err(|error| format!("owner REST identity: {error}"))?;
        #[cfg(test)]
        if crate::services::tui_o::ownership::execution::tests::mutant("scope") {
            return Ok(Arc::new(GatewayPort::new(http, user.id.get())));
        }
        let port = GatewayPort::new(http, user.id.get()).with_owner_scope(Scope {
            pool: permit.pool.clone(),
            provider: permit.identity.provider().into(),
        });
        Ok(Arc::new(port))
    };
    tokio::time::timeout(Duration::from_secs(5), Box::pin(prepare))
        .await
        .map_err(|_| "owner REST preparation timed out".to_string())?
}

pub(super) async fn facts(permit: &OwnerPermit) -> Result<FencedFacts, String> {
    let (hold, rows, queued_bodies) = crate::db::o_channel_activation::owner::facts(
        &permit.pool,
        permit.identity.provider(),
        permit.identity.raw_channel_id(),
        &permit.executor,
        permit.generation,
    )
    .await
    .map_err(|error| format!("channel execution fence: {error}"))?;
    Ok(FencedFacts {
        hold,
        facts: ActivationFacts {
            open_intake: rows.open_intake,
            runner_sessions: rows.foreign_sessions,
            node_override: None,
        },
        queued_bodies,
    })
}
