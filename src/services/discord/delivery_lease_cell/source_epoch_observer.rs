use crate::services::cluster::stream_relay::*;
use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex},
};

const SOURCE_EPOCH_CAPACITY: usize = 1024;

#[derive(Clone, Copy)]
struct Observation {
    witness: (SourceWitness, SourceFileIdentity),
    epoch: u64,
    touch: u64,
}

#[derive(Default)]
struct ObserverCache {
    observations: HashMap<String, Observation>,
    touch: u64,
}

impl ObserverCache {
    fn observe(
        &mut self,
        session: &str,
        marker: SourceWitness,
        file: SourceFileIdentity,
    ) -> SourceStamp {
        self.touch = self.touch.saturating_add(1);
        let witness = (marker, file);
        if !self.observations.contains_key(session) {
            if self.observations.len() == SOURCE_EPOCH_CAPACITY {
                let evicted = self
                    .observations
                    .iter()
                    .min_by(|(left_session, left), (right_session, right)| {
                        left.touch
                            .cmp(&right.touch)
                            .then_with(|| left_session.cmp(right_session))
                    })
                    .map(|(session, _)| session.clone())
                    .expect("full observer cache has an entry");
                self.observations.remove(&evicted);
            }
            self.observations.insert(
                session.into(),
                Observation {
                    witness,
                    epoch: 0,
                    touch: self.touch,
                },
            );
        }
        let observation = self
            .observations
            .get_mut(session)
            .expect("observation inserted");
        observation.touch = self.touch;
        if observation.witness != witness {
            observation.witness = witness;
            observation.epoch = observation.epoch.saturating_add(1);
        }
        SourceStamp {
            epoch: SourceEpoch::from_observation(observation.epoch),
            file,
            witness: marker,
        }
    }
}

static SOURCE_EPOCHS: LazyLock<Mutex<ObserverCache>> =
    LazyLock::new(|| Mutex::new(ObserverCache::default()));

/// Observer-only hybrid witness from one opened generation fd and the SHA-256 spawn nonce; either half may be absent, and it never decides delivery.
pub(in crate::services::discord) fn read_source_epoch_witness(session: &str) -> SourceWitness {
    let generation = crate::services::tmux_common::resolve_session_temp_path(session, "generation")
        .and_then(|path| std::fs::File::open(path).ok())
        .and_then(|file| {
            let metadata = file.metadata().ok()?;
            let mtime_ns = metadata
                .modified()
                .ok()?
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
                .and_then(|value| i64::try_from(value.as_nanos()).ok())?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                Some(GenerationSourceIdentity::Unix {
                    mtime_ns,
                    dev: metadata.dev(),
                    ino: metadata.ino(),
                })
            }
            #[cfg(not(unix))]
            {
                Some(GenerationSourceIdentity::Unsupported { mtime_ns })
            }
        });
    let incarnation =
        super::super::tmux::execution_identity::SessionIncarnationRef::capture(session);
    let spawn_nonce_hash = incarnation.spawn_nonce().map(|nonce| {
        use sha2::{Digest, Sha256};
        Sha256::digest(nonce.as_bytes()).into()
    });
    SourceWitness {
        generation,
        spawn_nonce_hash,
    }
}

pub(in crate::services::discord) fn marker_if_enabled(session: &str) -> Option<SourceWitness> {
    crate::config_live_reload::current()
        .map(|config| config.runtime.publication_permit_mode)
        .unwrap_or_default()
        .records_publication_observations()
        .then(|| read_source_epoch_witness(session))
}

pub(in crate::services::discord) fn source_stamp(
    session: &str,
    marker: SourceWitness,
    file: SourceFileIdentity,
) -> Option<SourceStamp> {
    (file != SourceFileIdentity::Unavailable
        && (marker.generation.is_some() || marker.spawn_nonce_hash.is_some()))
    .then(|| {
        SOURCE_EPOCHS
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .observe(session, marker, file)
    })
}

#[cfg(test)]
pub(super) fn assert_bounded_cache_eviction() {
    let mut cache = ObserverCache::default();
    let witness = |nonce| SourceWitness {
        generation: None,
        spawn_nonce_hash: Some([nonce; 32]),
    };
    let file = SourceFileIdentity::Unavailable;
    cache.observe("evicted", witness(1), file);
    let before_eviction = cache.observe("evicted", witness(2), file);
    assert_eq!(before_eviction.epoch, SourceEpoch::from_observation(1));
    for index in 0..SOURCE_EPOCH_CAPACITY {
        let session = format!("session-{index:04}");
        cache.observe(&session, witness(1), file);
    }
    assert_eq!(cache.observations.len(), SOURCE_EPOCH_CAPACITY);
    let after_eviction = cache.observe("evicted", witness(2), file);
    assert_eq!(after_eviction.epoch, SourceEpoch::from_observation(0));
    assert_eq!(cache.observations.len(), SOURCE_EPOCH_CAPACITY);
}
