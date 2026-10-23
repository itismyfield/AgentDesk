//! Residual delegation status reads retained until their callers are removed.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, PoisonError};

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
