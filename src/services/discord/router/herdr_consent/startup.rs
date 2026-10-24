//! Recovery owns interruption authority before this process admits consented apply.

use std::sync::Arc;

use crate::config::session_hosts::{self, EndpointMode};
use crate::services::discord::SharedData;
use crate::services::provider::ProviderKind;
use crate::services::session_host::policy::consent::{
    ConsentStore, State,
    boot_recovery::BootState,
    engine::{self, Event},
};
use poise::serenity_prelude as serenity;

pub(super) use crate::services::session_host::policy::consent::boot_recovery::PROCESS as BOOT;
static RECOVERY: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Local {
    node: String,
    endpoints: Vec<(std::path::PathBuf, String)>,
}

fn selected() -> Option<Local> {
    session_hosts::with_boot(|boot| {
        let boot = boot?;
        let node = boot.local_node()?;
        let endpoints: Vec<_> = boot
            .config()
            .herdr
            .endpoints
            .values()
            .filter(|endpoint| {
                endpoint.mode == EndpointMode::UserDefault && endpoint.execution_node.trim() == node
            })
            .map(|endpoint| {
                (
                    endpoint.socket_path.clone(),
                    endpoint.herdr_session.trim().to_owned(),
                )
            })
            .collect();
        (!endpoints.is_empty()).then(|| Local {
            node: node.into(),
            endpoints,
        })
    })
}

fn interrupt(store: &ConsentStore, local: &Local, boot: &BootState) -> Result<(), String> {
    if let Some(authority) = boot.try_claim() {
        // SAFETY: geteuid has no preconditions and cannot fail.
        let user = unsafe { libc::geteuid() };
        #[cfg(test)]
        if std::ptr::eq(boot, &BOOT) {
            super::STORE_READS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        let mut failures = Vec::new();
        for result in store.load_each()? {
            let consent = match result {
                Ok(consent) => consent,
                Err(error) => {
                    failures.push(error);
                    continue;
                }
            };
            if consent.state == State::Accepted
                && consent.key.node == local.node
                && consent.key.user == user
                && local.endpoints.iter().any(|(socket, session)| {
                    *socket == consent.key.socket && *session == consent.key.named_session
                })
                && let Err(error) =
                    engine::recover(store, Event::BootRecover(consent.key, &authority))
            {
                failures.push(error);
            }
        }
        if !failures.is_empty() {
            return Err(failures.join("; "));
        }
        authority.complete();
    }
    boot.is_ready()
        .then_some(())
        .ok_or_else(|| "consent boot recovery unavailable; preserve held inputs and restart after repairing the policy store".into())
}

pub(in crate::services::discord) async fn restore(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
) -> Result<(), String> {
    restore_excluding(http, shared, provider, &std::collections::BTreeSet::new()).await
}

pub(in crate::services::discord) async fn restore_excluding(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    blocked: &std::collections::BTreeSet<u64>,
) -> Result<(), String> {
    let Some(local) = selected() else {
        return Ok(());
    };
    let _recovery = RECOVERY.lock().await;
    let interrupted =
        tokio::task::spawn_blocking(move || interrupt(&super::store(), &local, &BOOT))
            .await
            .map_err(|error| format!("consent boot recovery: {error}"))
            .and_then(|result| result);
    let replayed = super::recover_excluding(http, shared, provider, blocked).await;
    match (interrupted, replayed) {
        (Ok(()), result) => result,
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(replay)) => Err(format!("{error}; consent replay also failed: {replay}")),
    }
}

#[cfg(test)]
#[path = "startup_tests.rs"]
mod tests;

#[cfg(test)]
pub(in crate::services::discord) fn store_reads_for_test() -> usize {
    super::STORE_READS.load(std::sync::atomic::Ordering::SeqCst)
}
