//! Boot evidence for Codex adoption. A witness exists only once a finished discovery pass read
//! its listings and bound every live source of the scope, and the role's recovery completed.

use std::collections::BTreeSet;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

use super::AdoptionRuntime;
use crate::services::discord::task_supervisor;
use crate::services::provider::ProviderKind;

/// The longest a boot read waits; elapsed time never becomes evidence.
pub(in crate::services::discord) const BOOT_WAIT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum BootRole {
    Gateway,
    RestWorker,
    /// Settles reconcile having skipped recovery, so v1 adopts nothing from it.
    Standby,
}

/// One live source a discovery pass read. `channel: None` is a source it could not place, which
/// may belong to any channel; `bound` only when the pass saw its binding connected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) struct LiveSource {
    pub key: String,
    pub channel: Option<u64>,
    pub bound: bool,
}

/// A finished pass: the live sources it read, or why a listing or the pass itself failed.
pub(in crate::services::discord) type DiscoveryPass = Result<Vec<LiveSource>, String>;

#[derive(Default)]
struct Progress {
    role: Option<BootRole>,
    /// The latest finished pass, numbered within this runtime.
    discovery: Option<(u64, DiscoveryPass)>,
    /// Set by recovery producers (U3-2a2); until then no witness is made.
    recovered: bool,
}

impl Progress {
    /// The latest pass's number when it read its listings and bound every source in `channels`;
    /// an unplaced unbound source blocks every scope.
    fn discovered(&self, channels: &BTreeSet<u64>) -> Option<u64> {
        let (pass, Ok(sources)) = self.discovery.as_ref()? else {
            return None;
        };
        let outside = |source: &LiveSource| source.channel.is_some_and(|c| !channels.contains(&c));
        sources
            .iter()
            .all(|source| source.bound || outside(source))
            .then_some(*pass)
    }

    fn ready(&self, channels: &BTreeSet<u64>) -> bool {
        match self.role {
            Some(BootRole::Standby) => true,
            Some(_) => self.recovered && self.discovered(channels).is_some(),
            None => false,
        }
    }
}

pub(in crate::services::discord) struct BootState {
    progress: watch::Sender<Progress>,
}

impl Default for BootState {
    fn default() -> Self {
        Self {
            progress: watch::Sender::new(Progress::default()),
        }
    }
}

/// The boot read for one scope. Its fields are private: only `wait_boot` makes one.
#[derive(Debug)]
pub(in crate::services::discord) struct CodexBootWitness {
    runtime: u64,
    provider: ProviderKind,
    bot: String,
    role: BootRole,
    channels: BTreeSet<u64>,
    pass: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum BootReadError {
    Unsupported(BootRole),
    Cancelled,
    TimedOut,
    /// The SharedData this runtime observed has ended.
    RuntimeGone,
}

impl AdoptionRuntime {
    /// Records a pass only after it returned: a spawned or running pass records nothing.
    #[cfg(unix)]
    pub(in crate::services::discord) fn record_discovery(&self, pass: DiscoveryPass) {
        let unbound: Vec<&str> = match &pass {
            Ok(sources) => sources
                .iter()
                .filter(|source| !source.bound)
                .map(|source| source.key.as_str())
                .collect(),
            Err(_) => Vec::new(),
        };
        tracing::debug!(runtime = self.id, failed = ?pass.as_ref().err(), ?unbound, "codex adoption discovery pass");
        self.boot.progress.send_modify(|progress| {
            let number = progress.discovery.as_ref().map_or(1, |(n, _)| n + 1);
            progress.discovery = Some((number, pass));
        });
    }

    /// Waits up to [`BOOT_WAIT`] for this boot's witness over `channels`; `stop` resolving
    /// cancels. Dropping the future cancels too, leaving nothing behind.
    pub(in crate::services::discord) async fn wait_boot(
        &self,
        channels: BTreeSet<u64>,
        stop: impl Future<Output = ()>,
    ) -> Result<CodexBootWitness, BootReadError> {
        let mut progress = self.boot.progress.subscribe();
        let read = tokio::select! {
            biased;
            () = stop => return Err(BootReadError::Cancelled),
            read = tokio::time::timeout(BOOT_WAIT, progress.wait_for(|p| p.ready(&channels))) => read,
        };
        let gone = self.shared.strong_count() == 0;
        let progress = match read {
            Ok(Ok(progress)) if !gone => progress,
            _ if gone => return Err(BootReadError::RuntimeGone),
            _ => return Err(BootReadError::TimedOut),
        };
        if let Some(role @ BootRole::Standby) = progress.role {
            return Err(BootReadError::Unsupported(role));
        }
        // `ready` held for this same snapshot, so role and pass are the ones it judged.
        let (Some(role), Some(pass)) = (progress.role, progress.discovered(&channels)) else {
            return Err(BootReadError::TimedOut);
        };
        Ok(CodexBootWitness {
            runtime: self.id,
            provider: self.provider.clone(),
            bot: self.bot.clone(),
            role,
            channels,
            pass,
        })
    }
}

#[cfg(test)]
impl AdoptionRuntime {
    /// Stands in for U3-2a2's recovery producers.
    pub(in crate::services::discord) fn record_recovery_for_tests(&self) {
        self.boot
            .progress
            .send_modify(|progress| progress.recovered = true);
    }
}

#[cfg(test)]
impl CodexBootWitness {
    pub(in crate::services::discord) fn pass_for_tests(&self) -> u64 {
        self.pass
    }
}

/// Records the boot's role. With a runtime installed it also logs the boot read for the
/// configured writer channels; the adoption host (U3-4) will read per channel instead.
pub(in crate::services::discord) fn record_role(
    runtime: Option<&Arc<AdoptionRuntime>>,
    role: BootRole,
) {
    let Some(runtime) = runtime.cloned() else {
        return;
    };
    runtime
        .boot
        .progress
        .send_modify(|progress| progress.role = Some(role));
    task_supervisor::spawn_observed("codex_adoption_boot_read", async move {
        let read = runtime
            .wait_boot(runtime.targets.clone(), std::future::pending())
            .await;
        match read {
            Ok(witness) => tracing::info!(
                runtime = witness.runtime,
                provider = witness.provider.as_str(),
                bot = %witness.bot,
                role = ?witness.role,
                channels = ?witness.channels,
                pass = witness.pass,
                "codex adoption boot witness"
            ),
            Err(error) => {
                tracing::info!(
                    runtime = runtime.id,
                    ?error,
                    "codex adoption boot witness unavailable"
                )
            }
        }
    });
}

#[cfg(test)]
#[path = "boot_tests.rs"]
mod tests;
