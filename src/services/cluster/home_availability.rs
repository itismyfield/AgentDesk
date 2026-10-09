//! Whether delegation can run for a provider, recorded only with
//! `runtime.channel_home_delegation_enabled` on and before any effect on a delegated channel. A
//! channel it refuses gets no turn and no POST instead of falling back to the gateway rules.

#![cfg_attr(not(test), allow(dead_code))]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, PoisonError};

/// Why delegated homes cannot run; each holds the channels it names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Unavailable {
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
    pub(crate) fn as_str(self) -> &'static str {
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

/// A provider's delegation as its boot judged it. `Off` is not `Ready`: with the switch off
/// nothing was checked, so it proves no preparation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Availability {
    Off,
    Ready,
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
    unavailable: Option<Unavailable>,
    channels: BTreeSet<u64>,
}

type Providers = BTreeMap<String, Provider>;

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
) {
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
            }
        }
    };
    with_providers(|providers| providers.insert(provider.to_owned(), record));
}

pub(crate) fn state(provider: &str) -> Availability {
    with_providers(|providers| match providers.get(provider) {
        None => Availability::Off,
        Some(Provider {
            unavailable: Some(reason),
            ..
        }) => Availability::Unavailable(*reason),
        Some(_) => Availability::Ready,
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
