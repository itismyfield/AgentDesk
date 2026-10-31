//! REST and first-init facts derive from the pinned channel execution permit.
use super::*;
use crate::services::cluster::intake_router_hook::owner_record::output::Scope;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Instant;

/// Bot identity each local REST client proved, keyed by provider and token fingerprint.
type Prepared = HashMap<(String, String), (u64, Instant)>;
static PREPARED: LazyLock<Mutex<Prepared>> = LazyLock::new(|| Mutex::new(HashMap::new()));
const PREPARED_FOR: Duration = Duration::from_secs(60);

fn key(provider: &str, http: &serenity::Http) -> (String, String) {
    let token = blake3::hash(http.token().as_bytes()).to_hex().to_string();
    (provider.to_owned(), token)
}

/// A REST client is ready only once it names its bot; a failure clears the earlier proof.
async fn identity(provider: &str, http: &serenity::Http) -> Result<u64, String> {
    let key = key(provider, http);
    if let Some((bot, at)) = PREPARED.lock().unwrap_or_else(|e| e.into_inner()).get(&key)
        && at.elapsed() < PREPARED_FOR
    {
        return Ok(*bot);
    }
    let result = match tokio::time::timeout(Duration::from_secs(3), http.get_current_user()).await {
        Ok(Ok(user)) => Ok(user.id.get()),
        Ok(Err(error)) => Err(format!("owner REST identity: {error}")),
        Err(_) => Err("owner REST identity timed out".to_string()),
    };
    let mut prepared = PREPARED.lock().unwrap_or_else(|e| e.into_inner());
    match &result {
        Ok(bot) => prepared.insert(key, (*bot, Instant::now())),
        Err(_) => prepared.remove(&key),
    };
    result
}

/// The routing capability publishes this provider-level result; channel ports reuse it.
pub(in crate::services::discord::runtime_bootstrap) async fn prepare(shared: &SharedData) -> bool {
    let Some(http) = shared.serenity_http_or_token_fallback() else {
        return false;
    };
    identity(shared.provider.as_str(), &http).await.is_ok()
}

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
        let bot = identity(permit.identity.provider(), &http).await?;
        #[cfg(test)]
        if crate::services::tui_o::ownership::execution::tests::mutant("scope") {
            return Ok(Arc::new(GatewayPort::new(http, bot)));
        }
        let port = GatewayPort::new(http, bot).with_owner_scope(Scope {
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
