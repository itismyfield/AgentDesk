//! Whether delegation can run for a provider, recorded only with the switch on and before any
//! effect on a delegated channel; a channel it refuses gets no turn and no POST, never the gateway's.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, PoisonError};

/// Why delegated homes cannot run; each holds the channels it names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unavailable {
    MissingPool,
    MissingInstanceId,
    InstanceIdMismatch,
    InvalidChannelRow,
    WriterNotSelected,
    EndpointMissing,
    EndpointNotLocal,
    RuntimeRootUnavailable,
    IntakeModeNotEnforce,
    DualAuthority,
    RestoreUnavailable,
}

impl Unavailable {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MissingPool => "missing_pool",
            Self::MissingInstanceId => "missing_instance_id",
            Self::InstanceIdMismatch => "instance_id_mismatch",
            Self::InvalidChannelRow => "invalid_channel_row",
            Self::WriterNotSelected => "writer_not_selected",
            Self::EndpointMissing => "endpoint_missing",
            Self::EndpointNotLocal => "endpoint_not_local",
            Self::RuntimeRootUnavailable => "runtime_root_unavailable",
            Self::IntakeModeNotEnforce => "intake_mode_not_enforce",
            Self::DualAuthority => "dual_authority",
            Self::RestoreUnavailable => "restore_unavailable",
        }
    }
}

impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Only boot identity checks, never proof of protection or scoped restore readiness.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Availability {
    Off,
    PreflightPassed,
    Unavailable(Unavailable),
}

/// The boot checks every delegated home needs: a PG pool and this node's `cluster.instance_id`.
pub(crate) fn preflight(has_pool: bool, instance_id: Option<&str>) -> Result<(), Unavailable> {
    if !has_pool {
        return Err(Unavailable::MissingPool);
    }
    match instance_id.map(str::trim) {
        Some(id) if !id.is_empty() => Ok(()),
        _ => Err(Unavailable::MissingInstanceId),
    }
}

#[derive(Default)]
struct Provider {
    identity: Arc<()>,
    unavailable: Option<Unavailable>,
    channels: BTreeSet<u64>,
}

type Providers = BTreeMap<String, Provider>;

/// This runtime's installation; stale exit cleanup cannot remove a replacement's judgement.
pub(crate) struct Registration {
    provider: String,
    identity: Arc<()>,
}

impl Drop for Registration {
    fn drop(&mut self) {
        with_providers(|providers| {
            if providers
                .get(&self.provider)
                .is_some_and(|p| Arc::ptr_eq(&p.identity, &self.identity))
            {
                providers.remove(&self.provider);
            }
        });
    }
}

pub(crate) fn install_enabled(
    provider: &str,
    switch: Option<bool>,
    judged: impl FnOnce() -> Result<(), Unavailable>,
    channels: impl FnOnce() -> BTreeSet<u64>,
) -> Option<Registration> {
    if switch != Some(true) {
        return None;
    }
    Some(install(provider, judged(), channels))
}

#[cfg(not(test))]
static PROVIDERS: Mutex<Providers> = Mutex::new(BTreeMap::new());
#[cfg(test)]
thread_local! {
    static PROVIDERS: Mutex<Providers> = const { Mutex::new(BTreeMap::new()) };
}

fn with_providers<R>(use_providers: impl FnOnce(&mut Providers) -> R) -> R {
    let locked = |providers: &Mutex<Providers>| {
        use_providers(&mut providers.lock().unwrap_or_else(PoisonError::into_inner))
    };
    #[cfg(not(test))]
    return locked(&PROVIDERS);
    #[cfg(test)]
    PROVIDERS.with(locked)
}

/// Records the provider's boot judgement; a failed one holds `channels`, the channels that may be
/// delegated, which are read only then.
pub(crate) fn install(
    provider: &str,
    judged: Result<(), Unavailable>,
    channels: impl FnOnce() -> BTreeSet<u64>,
) -> Registration {
    let record = match judged {
        Ok(()) => Provider::default(),
        Err(reason) => {
            tracing::error!(
                provider,
                reason = reason.as_str(),
                "channel home delegation unavailable; its channels are held"
            );
            Provider {
                unavailable: Some(reason),
                channels: channels(),
                ..Provider::default()
            }
        }
    };
    let provider = provider.to_owned();
    let identity = Arc::clone(&record.identity);
    with_providers(|providers| providers.insert(provider.clone(), record));
    Registration { provider, identity }
}

pub fn state(provider: &str) -> Availability {
    with_providers(|providers| match providers.get(provider) {
        None => Availability::Off,
        Some(Provider {
            unavailable: Some(reason),
            ..
        }) => Availability::Unavailable(*reason),
        Some(_) => Availability::PreflightPassed,
    })
}

pub(crate) fn held_channels(provider: &str) -> BTreeSet<u64> {
    with_providers(|providers| {
        providers
            .get(provider)
            .map(|p| p.channels.clone())
            .unwrap_or_default()
    })
}

/// Why `channel` takes no turn and no POST here; `None` for every channel while nothing was
/// recorded, as with the switch off.
pub(crate) fn refusal(channel: u64) -> Option<Unavailable> {
    with_providers(|providers| {
        let mut held = providers.values().filter(|p| p.channels.contains(&channel));
        held.find_map(|provider| provider.unavailable)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_does_not_prepare_or_select_channels() {
        for switch in [None, Some(false)] {
            assert!(
                install_enabled(
                    "claude",
                    switch,
                    || panic!("off must not preflight"),
                    || panic!("off must not select channels"),
                )
                .is_none()
            );
        }
        assert_eq!(state("claude"), Availability::Off);
    }

    #[test]
    fn runtime_exit_clears_only_its_own_availability_and_off_can_reenter() {
        let old = install("claude", Err(Unavailable::MissingPool), || [7].into());
        let current = install("claude", Err(Unavailable::MissingInstanceId), || [8].into());
        drop(old);
        assert_eq!(refusal(7), None);
        assert_eq!(refusal(8), Some(Unavailable::MissingInstanceId));
        drop(current);
        assert!(
            install_enabled(
                "claude",
                Some(false),
                || panic!("off must not preflight"),
                Default::default,
            )
            .is_none()
        );
        assert_eq!(state("claude"), Availability::Off);
        assert_eq!(refusal(8), None);
    }
}
