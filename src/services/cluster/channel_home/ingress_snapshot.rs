//! A copy of this node's gate for one channel, for the ingress shadow. It never waits, expires,
//! prunes, closes or initializes anything: each lock is tried once and the sample given up if taken.

use std::sync::TryLockError;

use tokio::time::Instant;

use super::{HOLD_FOR, HomeGate, HomeOwnership, with_homes};
use crate::services::cluster::channel_ingress::{IngressKey, LocalLeaseSample};
use crate::services::tui_o::ownership::GatewayOwnership;

/// The lock a sample could not take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SampleLock {
    Registry,
    Local,
    Inner,
}

#[derive(Clone, Debug)]
pub(crate) enum GateSample {
    Sampled(LocalLeaseSample),
    /// No registry, or no gate for the channel in it.
    NotRegistered,
    Busy(SampleLock),
    Poisoned(SampleLock),
}

impl GateSample {
    pub(crate) fn sample(&self) -> Option<&LocalLeaseSample> {
        match self {
            Self::Sampled(sample) => Some(sample),
            Self::NotRegistered | Self::Busy(_) | Self::Poisoned(_) => None,
        }
    }

    /// Whether a lock was held or poisoned, so nothing could be read.
    pub(crate) fn unreadable(&self) -> bool {
        matches!(self, Self::Busy(_) | Self::Poisoned(_))
    }

    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::Sampled(_) => "sampled",
            Self::NotRegistered => "not_registered",
            Self::Busy(SampleLock::Registry) => "registry_busy",
            Self::Busy(SampleLock::Local) => "local_busy",
            Self::Busy(SampleLock::Inner) => "inner_busy",
            Self::Poisoned(SampleLock::Registry) => "registry_poisoned",
            Self::Poisoned(SampleLock::Local) => "local_poisoned",
            Self::Poisoned(SampleLock::Inner) => "inner_poisoned",
        }
    }
}

#[cfg(test)]
thread_local! {
    static SAMPLED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// How many samples this thread asked for, so a test can show a dormant path takes none.
#[cfg(test)]
pub(crate) fn sampled_calls() -> usize {
    SAMPLED.with(std::cell::Cell::get)
}

fn tried<T>(result: Result<T, TryLockError<T>>, lock: SampleLock) -> Result<T, GateSample> {
    result.map_err(|error| match error {
        TryLockError::WouldBlock => GateSample::Busy(lock),
        TryLockError::Poisoned(_) => GateSample::Poisoned(lock),
    })
}

/// The registered gate of `channel`, keyed by the provider the home row names.
pub(crate) fn try_sample(channel: u64, row_provider: &str) -> GateSample {
    #[cfg(test)]
    SAMPLED.with(|calls| calls.set(calls.get() + 1));
    let gate = with_homes(|homes| {
        let Some(registry) = homes.get() else {
            return Ok(None);
        };
        let registry = tried(registry.try_lock(), SampleLock::Registry)?;
        Ok(registry.gates.get(&channel.to_string()).cloned())
    });
    match gate {
        Ok(Some(gate)) => sample_gate(&gate, row_provider),
        Ok(None) => GateSample::NotRegistered,
        Err(refused) => refused,
    }
}

/// One gate read under a single hold of its lock, so epoch, intake and deadline come from one
/// state; also the port tests on other threads drive, as the registry is per thread there.
pub(crate) fn sample_gate(gate: &HomeGate, row_provider: &str) -> GateSample {
    let channel = gate.channel_id.parse().unwrap_or_default();
    let Some(key) = IngressKey::parse(row_provider, channel) else {
        return GateSample::NotRegistered;
    };
    let local = match tried(gate.local.try_lock(), SampleLock::Local) {
        Ok(local) => local,
        Err(refused) => return refused,
    };
    let (ownership, expires_at) = match local.held.as_ref() {
        None => (HomeOwnership::Lost, None),
        Some(held) => match gate.gate.try_current() {
            Err(TryLockError::WouldBlock) => return GateSample::Busy(SampleLock::Inner),
            Err(TryLockError::Poisoned(_)) => return GateSample::Poisoned(SampleLock::Inner),
            Ok(inner)
                if inner
                    == (GatewayOwnership::Owned {
                        epoch: held.gate_epoch,
                    }) =>
            {
                let ownership = HomeOwnership::Owned {
                    home_epoch: held.home_epoch,
                    gate_epoch: held.gate_epoch,
                    intake: gate.intake_of(&local, held.home_epoch),
                };
                (ownership, Some(held.renew_sent + HOLD_FOR))
            }
            Ok(_) => (HomeOwnership::Lost, None),
        },
    };
    let sampled_at = Instant::now();
    drop(local);
    GateSample::Sampled(LocalLeaseSample {
        key,
        holder: gate.holder.clone(),
        ownership,
        sampled_at,
        expires_at,
    })
}

#[cfg(test)]
mod tests {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::Arc;
    use std::time::Duration;

    use super::super::{HomeIntake, lock_homes, register, unregister};
    use super::*;
    use crate::db::o_channel_homes::{HeldHome, HomeState};

    const CH: u64 = 5_713_000_000_000_101;

    fn held(channel: u64, epoch: i64, sent: Instant) -> Arc<HomeGate> {
        let gate = Arc::new(HomeGate::new(&channel.to_string(), "node-a"));
        let written = HeldHome::for_test(&channel.to_string(), "node-a", epoch, HomeState::Worker);
        gate.confirm(&written, sent).unwrap();
        gate
    }

    fn owned(sample: &GateSample) -> Option<(i64, u64, HomeIntake, Option<Instant>)> {
        match sample.sample()? {
            LocalLeaseSample {
                ownership:
                    HomeOwnership::Owned {
                        home_epoch,
                        gate_epoch,
                        intake,
                    },
                expires_at,
                ..
            } => Some((*home_epoch, *gate_epoch, *intake, *expires_at)),
            _ => None,
        }
    }

    fn commands_listed(channel: u64) -> bool {
        with_homes(|homes| {
            let registry = lock_homes(homes.get().unwrap());
            registry.commands.contains_key(&channel.to_string())
        })
    }

    #[tokio::test(start_paused = true)]
    async fn sampling_expires_prunes_closes_and_initializes_nothing() {
        assert!(matches!(try_sample(CH, "codex"), GateSample::NotRegistered));
        assert!(
            with_homes(|homes| homes.get().is_none()),
            "no registry made"
        );

        let sent = Instant::now();
        let gate = held(CH, 4, sent);
        register(Arc::clone(&gate));
        let listed = held(CH + 1, 1, sent);
        register(Arc::clone(&listed));
        let permit = listed.admit_command("codex").expect("held gate admits");
        unregister(&(CH + 1).to_string());
        drop(permit);
        assert!(
            commands_listed(CH + 1),
            "an idle counter waits for the next prune"
        );

        tokio::time::advance(HOLD_FOR + Duration::from_secs(1)).await;
        let sample = try_sample(CH, "codex");
        assert_eq!(
            owned(&sample),
            Some((4, 1, HomeIntake::Open, Some(sent + HOLD_FOR))),
            "a lapsed hold is copied, never expired: {sample:?}"
        );
        assert_eq!(gate.expiry(), Some(sent + HOLD_FOR), "hold kept");
        assert_eq!(
            gate.gate().current(),
            GatewayOwnership::Owned { epoch: 1 },
            "inner gate still open"
        );
        assert!(commands_listed(CH + 1), "registry not pruned");
        let local = gate.local.lock().unwrap();
        assert_eq!(
            (local.intake_closed, local.closed_at, local.withdrawn),
            (None, None, false),
            "nothing closed"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_taken_or_poisoned_lock_gives_no_sample() {
        let gate = held(CH, 4, Instant::now());
        register(Arc::clone(&gate));
        assert!(
            owned(&try_sample(CH, "codex")).is_some(),
            "free locks sample"
        );

        with_homes(|homes| {
            let _registry = lock_homes(homes.get().unwrap());
            let busy = try_sample(CH, "codex");
            assert!(
                matches!(busy, GateSample::Busy(SampleLock::Registry)),
                "{busy:?}"
            );
        });
        {
            let _local = gate.local.lock().unwrap();
            let busy = try_sample(CH, "codex");
            assert!(
                matches!(busy, GateSample::Busy(SampleLock::Local)),
                "{busy:?}"
            );
        }

        // A writer's admission holds the inner lock; the sample must come back at once.
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let inner = gate.gate();
        let admitting = std::thread::spawn(move || {
            inner.admit(|_| {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            })
        });
        entered_rx.recv().unwrap();
        let (sampled_tx, sampled_rx) = std::sync::mpsc::channel();
        let sampling = Arc::clone(&gate);
        let sampler = std::thread::spawn(move || {
            sampled_tx.send(sample_gate(&sampling, "codex")).unwrap();
        });
        let busy = sampled_rx.recv_timeout(Duration::from_secs(2));
        release_tx.send(()).unwrap();
        admitting.join().unwrap();
        sampler.join().unwrap();
        let busy = busy.expect("the sample waited on the inner lock");
        assert!(
            matches!(busy, GateSample::Busy(SampleLock::Inner)),
            "{busy:?}"
        );

        let poison = |lock: &dyn Fn()| {
            let _ = catch_unwind(AssertUnwindSafe(lock));
        };
        poison(&|| {
            let _inner = gate.gate().admit(|_| -> u8 { panic!("poison inner") });
        });
        let poisoned = try_sample(CH, "codex");
        assert!(
            matches!(poisoned, GateSample::Poisoned(SampleLock::Inner)),
            "{poisoned:?}"
        );
        poison(&|| {
            let _local = gate.local.lock();
            panic!("poison local");
        });
        let poisoned = try_sample(CH, "codex");
        assert!(
            matches!(poisoned, GateSample::Poisoned(SampleLock::Local)),
            "{poisoned:?}"
        );
        poison(&|| {
            with_homes(|homes| {
                let _registry = homes.get().unwrap().lock();
                panic!("poison registry");
            })
        });
        let poisoned = try_sample(CH, "codex");
        assert!(
            matches!(poisoned, GateSample::Poisoned(SampleLock::Registry)),
            "{poisoned:?}"
        );
        for sample in [busy, poisoned] {
            assert!(sample.sample().is_none() && sample.unreadable());
        }
    }

    /// Renewals to new epochs, intake closes and lapses race the sampler; every sample must be one
    /// state: the epoch's own deadline and gate epoch, never one epoch's with another's.
    #[test]
    fn a_sample_never_mixes_two_states_of_one_gate() {
        const EPOCHS: i64 = 4_000;
        let gate = Arc::new(HomeGate::new(&CH.to_string(), "node-a"));
        let sends = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (writer_gate, writer_sends, writer_done) =
            (Arc::clone(&gate), Arc::clone(&sends), Arc::clone(&done));
        let writer = std::thread::spawn(move || {
            for epoch in 1..=EPOCHS {
                let written =
                    HeldHome::for_test(&CH.to_string(), "node-a", epoch, HomeState::Worker);
                // A send no later than the last lapse is refused as late; send again.
                loop {
                    let sent = Instant::now();
                    writer_sends.lock().unwrap().insert(epoch, sent);
                    if writer_gate.confirm(&written, sent).is_ok() {
                        break;
                    }
                }
                if epoch % 3 == 0 {
                    writer_gate.close_intake();
                }
                if epoch % 5 == 0 {
                    writer_gate.renewal_stale(epoch);
                }
            }
            writer_done.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        let (mut owned_seen, mut epochs_seen) = (0, std::collections::BTreeSet::new());
        let mut mixed = Vec::new();
        while !done.load(std::sync::atomic::Ordering::SeqCst) {
            let sample = sample_gate(&gate, "codex");
            let Some((epoch, gate_epoch, intake, expires_at)) = owned(&sample) else {
                continue;
            };
            owned_seen += 1;
            epochs_seen.insert(epoch);
            let sent = sends.lock().unwrap().get(&epoch).copied();
            let closed = intake == HomeIntake::Closed;
            // Every new epoch is one more acquisition of the inner gate.
            if expires_at != sent.map(|sent| sent + HOLD_FOR)
                || i64::try_from(gate_epoch) != Ok(epoch)
                || (closed && epoch % 3 != 0)
            {
                mixed.push((epoch, gate_epoch, intake, expires_at, sent));
            }
        }
        writer.join().unwrap();
        assert!(
            mixed.is_empty(),
            "mixed samples: {:?}",
            &mixed[..mixed.len().min(5)]
        );
        assert!(
            owned_seen > 100 && epochs_seen.len() > 10,
            "the sampler raced the writer: {owned_seen} samples over {} epochs",
            epochs_seen.len()
        );
    }
}
