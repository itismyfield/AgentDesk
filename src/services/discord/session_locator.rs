use sqlx::PgPool;

use super::{SharedData, adk_session};
use crate::db::dispatched_session_canonical_identity::{
    CanonicalSessionIdentity, SessionIdentityKind, resolve_session_row_pg,
};
use crate::services::provider::ProviderKind;

fn identity<'a>(hash: &'a str, channel: &'a str) -> CanonicalSessionIdentity<'a> {
    CanonicalSessionIdentity {
        kind: SessionIdentityKind::DiscordChannel,
        discord_token_hash: hash,
        channel_id: channel,
    }
}

pub(super) async fn current_row(
    pool: &PgPool,
    hash: &str,
    provider: &ProviderKind,
    channel: u64,
) -> Result<Option<(i64, String)>, String> {
    current_row_typed(pool, hash, provider, channel)
        .await
        .map_err(|error| format!("session locator: {error:?}"))
}

pub(super) async fn current_row_typed(
    pool: &PgPool,
    hash: &str,
    provider: &ProviderKind,
    channel: u64,
) -> Result<
    Option<(i64, String)>,
    crate::db::dispatched_session_canonical_identity::HookSessionUpsertError,
> {
    resolve_session_row_pg(
        pool,
        None,
        Some(provider.as_str()),
        Some(identity(hash, &channel.to_string())),
    )
    .await
}

/// Both wire formats refer to the one stored actual name; all read evidence must converge.
pub(super) fn candidates(
    pool: Option<&PgPool>,
    hash: &str,
    provider: &ProviderKind,
    name: &str,
    channel: Option<u64>,
) -> Result<[String; 2], String> {
    let raw = adk_session::build_session_key_candidates(hash, provider, name);
    let channel = channel
        .or_else(|| crate::services::tui_prompt_dedupe::owner_channel_for_tmux_session(name));
    let Some(pool) = pool else {
        return Ok(raw);
    };
    let (hash, provider) = (hash.to_owned(), provider.clone());
    crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |pool| async move {
            let channel = channel.map(|id| id.to_string());
            let canonical = channel.as_deref().map(|channel| identity(&hash, channel));
            let mut found = None;
            for key in &raw {
                if let Some(row) =
                    resolve_session_row_pg(&pool, Some(key), Some(provider.as_str()), canonical)
                        .await
                        .map_err(|error| format!("session locator: {error:?}"))?
                {
                    if found
                        .as_ref()
                        .is_some_and(|existing: &(i64, String)| existing != &row)
                    {
                        return Err("session locator evidence diverged".into());
                    }
                    found = Some(row);
                }
            }
            let Some((id, primary)) = found else {
                return Ok(raw);
            };
            let owner: (Option<String>, Option<String>, Option<String>) = sqlx::query_as(
                "SELECT provider, discord_token_hash, channel_id FROM sessions WHERE id = $1",
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .map_err(|error| error.to_string())?;
            if owner
                .0
                .as_deref()
                .is_some_and(|value| value != provider.as_str())
                || owner.1.as_deref().is_some_and(|value| value != hash)
                || channel
                    .as_deref()
                    .zip(owner.2.as_deref())
                    .is_some_and(|(expected, actual)| expected != actual)
            {
                return Err("session locator ownership does not match".into());
            }
            let parsed = super::session_identity::SessionIdentity::parse(&primary)
                .ok_or("invalid primary locator")?;
            let alternate = if parsed.namespaced {
                format!("{}:{}", parsed.host, parsed.tmux_name)
            } else {
                format!(
                    "{}/{}/{}:{}",
                    provider.as_str(),
                    hash,
                    parsed.host,
                    parsed.tmux_name
                )
            };
            if resolve_session_row_pg(&pool, Some(&alternate), Some(provider.as_str()), canonical)
                .await
                .map_err(|error| format!("session locator: {error:?}"))?
                .is_some_and(|row| row.0 != id)
            {
                return Err("alternate actual locator belongs to another row".into());
            }
            Ok([primary, alternate])
        },
        |error| error,
    )
}

pub(super) async fn channel_key(
    shared: &SharedData,
    provider: &ProviderKind,
    channel: u64,
    preferred: String,
) -> Option<String> {
    let Some(pool) = shared.pg_pool.as_ref() else {
        return Some(preferred);
    };
    let row = current_row(pool, &shared.token_hash, provider, channel)
        .await
        .ok()?;
    let Some((id, primary)) = row else {
        return Some(preferred);
    };
    #[cfg(unix)]
    if matches!(provider, ProviderKind::Claude | ProviderKind::Codex)
        && unix::observe_old(id, &primary, channel)
    {
        return Some(preferred);
    }
    #[cfg(not(unix))]
    let _ = id;
    Some(primary)
}

pub(super) async fn origin_deferred(
    shared: &SharedData,
    provider: &ProviderKind,
    channel: u64,
    requested: &str,
) -> bool {
    let Some(pool) = shared.pg_pool.as_ref() else {
        return false;
    };
    match current_row(pool, &shared.token_hash, provider, channel).await {
        Ok(Some((_, primary))) => primary != requested,
        Ok(None) => false,
        Err(_) => true,
    }
}

#[cfg(unix)]
pub(crate) use unix::{RegistrationScope, created};

#[cfg(unix)]
mod unix {
    use super::*;
    use crate::db::dispatched_session_canonical_identity::key_promotion::{
        RegistrationSnapshot, RegistrationWitness, discover_registration_pg,
        register_hook_session_pg,
    };
    use crate::services::platform::tmux::session_inventory::{self, Inventory, Observation};
    use crate::services::tui_prompt_dedupe::binding_context::BindingContext;
    use std::{
        cell::RefCell,
        collections::HashMap,
        sync::{LazyLock, Mutex},
    };

    static OLD_HOSTS: LazyLock<Mutex<HashMap<(i64, String), Observation>>> =
        LazyLock::new(Default::default);
    thread_local! { static REGISTRATION: RefCell<Vec<Option<Pending>>> = const { RefCell::new(Vec::new()) }; }
    #[derive(Clone)]
    struct Pending {
        pool: PgPool,
        snapshot: RegistrationSnapshot,
        hash: String,
        provider: String,
        channel: u64,
        key: String,
        nonce: String,
        dispatched_origin: bool,
        expected: Observation,
    }
    fn missing(observed: &Observation, name: &str) -> bool {
        observed.current()
            && match &observed.inventory {
                Inventory::Empty(_) => true,
                Inventory::Sessions(names) => !names.iter().any(|entry| entry == name),
                Inventory::ProbeFailed(_) => false,
            }
    }
    fn local_name(key: &str) -> Option<String> {
        let parsed = super::super::session_identity::SessionIdentity::parse(key)?;
        (parsed.host == crate::services::platform::session_host_label()).then_some(parsed.tmux_name)
    }
    pub(super) fn observe_old(id: i64, key: &str, channel: u64) -> bool {
        let Some(name) = local_name(key) else {
            return false;
        };
        let observed = session_inventory::observe_inventory(channel);
        let mut old = OLD_HOSTS.lock().unwrap_or_else(|error| error.into_inner());
        if observed.current()
            && matches!(&observed.inventory, Inventory::Sessions(names) if names.contains(&name))
        {
            old.insert((id, key.to_owned()), observed);
            return false;
        }
        old.get(&(id, key.to_owned())).is_some_and(|expected| {
            expected.current() && expected.same_namespace(&observed) && missing(&observed, &name)
        })
    }

    pub(crate) struct RegistrationScope {
        active: bool,
        _completion: crate::services::tui_prompt_dedupe::binding_context::registration::Scope,
    }
    impl RegistrationScope {
        pub(crate) fn enter(
            pool: Option<&PgPool>,
            hash: &str,
            provider: &ProviderKind,
            channel: u64,
            name: Option<&str>,
            nonce: Option<&str>,
            origin: Option<bool>,
        ) -> Result<Self, String> {
            REGISTRATION.with(|slot| slot.borrow_mut().push(None));
            let guard = Self {
                active: true,
                _completion:
                    crate::services::tui_prompt_dedupe::binding_context::registration::enter(
                        created,
                    ),
            };
            let Some(dispatched_origin) = origin else {
                return Ok(guard);
            };
            let (Some(pool), Some(name)) = (pool, name) else {
                return Ok(guard);
            };
            let nonce = nonce.unwrap_or_default();
            let key = adk_session::build_namespaced_session_key(hash, provider, name);
            let (hash, provider, nonce) = (
                hash.to_owned(),
                provider.as_str().to_owned(),
                nonce.to_owned(),
            );
            let (query_hash, query_provider) = (hash.clone(), provider.clone());
            let snapshot = crate::utils::async_bridge::block_on_pg_result(
                pool,
                move |pool| async move {
                    discover_registration_pg(
                        &pool,
                        &query_provider,
                        identity(&query_hash, &channel.to_string()),
                    )
                    .await
                    .map_err(|error| format!("registration discovery: {error:?}"))
                },
                |error| error,
            )?;
            let Some(snapshot) = snapshot.filter(|row| !crate::db::dispatched_session_canonical_identity::key_promotion::same_physical_locator(&row.primary, &key)) else {
                return Ok(guard);
            };
            let expected = OLD_HOSTS
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .get(&(snapshot.id, snapshot.primary.clone()))
                .cloned()
                .ok_or("old host namespace is unknown")?;
            if snapshot.hosted.is_some() || nonce.trim().is_empty() {
                return Err("registration is not a fresh local tmux execution".into());
            }
            let pin =
                crate::db::dispatched_session_canonical_identity::HookSessionActorPin::Existing {
                    id: snapshot.id,
                    active_turn_nonce: snapshot.nonce.clone(),
                    status: snapshot.status.clone(),
                };
            if pin.active_for_other_actor(Some(&nonce)) {
                return Err("old actor still owns the registration".into());
            }
            let old_name = local_name(&snapshot.primary).ok_or("old host is not local")?;
            let observed = session_inventory::observe_inventory(channel);
            if !expected.current()
                || !expected.same_namespace(&observed)
                || !missing(&observed, &old_name)
            {
                return Err("old execution is live or its namespace is unknown".into());
            }
            if let Some(state) = super::super::inflight::load_inflight_state_read_only_result(
                provider_kind(&provider)?,
                channel,
            )
            .map_err(|error| format!("inflight witness: {error}"))?
                && state.turn_nonce.as_deref() != Some(&nonce)
            {
                return Err("old inflight actor still owns the channel".into());
            }
            REGISTRATION.with(|slot| {
                if let Some(frame) = slot.borrow_mut().last_mut() {
                    *frame = Some(Pending {
                        pool: pool.clone(),
                        snapshot,
                        hash,
                        provider,
                        channel,
                        key,
                        nonce,
                        dispatched_origin,
                        expected,
                    });
                }
            });
            Ok(guard)
        }
    }
    impl Drop for RegistrationScope {
        fn drop(&mut self) {
            if self.active {
                REGISTRATION.with(|slot| {
                    slot.borrow_mut().pop();
                });
            }
        }
    }

    fn provider_kind(provider: &str) -> Result<&'static ProviderKind, String> {
        static CLAUDE: ProviderKind = ProviderKind::Claude;
        static CODEX: ProviderKind = ProviderKind::Codex;
        match provider {
            "claude" => Ok(&CLAUDE),
            "codex" => Ok(&CODEX),
            _ => Err("registration provider unsupported".into()),
        }
    }

    /// The creator calls this only after publishing this incarnation's spawn marker.
    pub(crate) fn created(context: &BindingContext) -> Result<(), String> {
        let Some(pending) = REGISTRATION.with(|slot| slot.borrow().last().cloned().flatten())
        else {
            return Ok(());
        };
        let name = super::super::session_identity::tmux_name_from_session_key(&pending.key)
            .ok_or("new key has no actual name")?;
        if context.tmux_session != name {
            return Ok(());
        }
        if context.provider != pending.provider
            || context.channel_id != Some(pending.channel)
            || context.execution_nonce.is_empty()
            || !matches!(context.launch_mode.as_str(), "fresh" | "resume")
        {
            return Err("created incarnation does not match canonical registration".into());
        }
        let old_name = local_name(&pending.snapshot.primary).ok_or("old host is not local")?;
        let cache_key = (pending.snapshot.id, pending.snapshot.primary.clone());
        let source_pool = pending.pool.clone();
        crate::utils::async_bridge::block_on_pg_result(
            &source_pool,
            move |pool| async move {
                let absent = || {
                    let observed = session_inventory::observe_inventory(pending.channel);
                    pending.expected.current()
                        && pending.expected.same_namespace(&observed)
                        && missing(&observed, &old_name)
                };
                let channel = pending.channel.to_string();
                let mut snapshot = pending.snapshot.clone();
                for attempt in 0..2 {
                    let witness = RegistrationWitness {
                        snapshot: &snapshot,
                        new_key: &pending.key,
                        new_nonce: &pending.nonce,
                        old_host_absent: &absent,
                    };
                    let params = crate::db::dispatched_sessions::HookSessionUpsert {
                        session_key: &pending.key,
                        instance_id: None,
                        agent_id: None,
                        provider: &pending.provider,
                        status: "turn_active",
                        session_info: None,
                        model: None,
                        tokens: None,
                        cwd: None,
                        active_dispatch_id: None,
                        thread_channel_id: None,
                        channel_id: Some(&channel),
                        claude_session_id: None,
                        raw_provider_session_id: None,
                        turn_start_nonce: Some(&pending.nonce),
                        dispatched_origin: pending.dispatched_origin,
                    };
                    let result = register_hook_session_pg(
                        &pool,
                        params,
                        identity(&pending.hash, &channel),
                        &witness,
                    )
                    .await;
                    match result {
                        Ok(outcome) => return Ok(outcome),
                        Err(error) if attempt == 0 && crate::db::dispatched_session_canonical_identity::key_promotion::snapshot_changed(&error) => {
                            let next = discover_registration_pg(&pool, &pending.provider, identity(&pending.hash, &channel)).await
                                .map_err(|error| format!("registration rediscovery: {error:?}"))?.ok_or("registration row disappeared")?;
                            if next.id != snapshot.id || next.primary != snapshot.primary { return Err("registration ownership changed".into()); }
                            snapshot = next;
                        }
                        Err(error) => return Err(format!("register created session: {error:?}")),
                    }
                }
                Err("registration did not settle".into())
            },
            |error| error,
        )?;
        OLD_HOSTS
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&cache_key);
        Ok(())
    }
}

#[cfg(all(test, unix))]
#[path = "session_locator_pg_tests.rs"]
pub(crate) mod tests;
