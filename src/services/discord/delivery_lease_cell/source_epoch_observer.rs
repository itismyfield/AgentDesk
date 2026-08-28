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

pub(in crate::services::discord) fn marker_if_enabled(session: &str) -> Option<SourceWitness> {
    crate::config_live_reload::current()
        .map(|config| config.runtime.publication_permit_mode)
        .unwrap_or_default()
        .records_publication_observations()
        .then(|| super::super::tmux::read_source_epoch_witness(session))
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
