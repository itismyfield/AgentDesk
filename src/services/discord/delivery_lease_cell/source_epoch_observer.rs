use crate::services::cluster::stream_relay::*;
use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex},
};
#[derive(Clone, Copy)]
struct Observation((SourceWitness, SourceFileIdentity), u64);
static SOURCE_EPOCHS: LazyLock<Mutex<HashMap<String, Observation>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

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
) -> SourceStamp {
    let mut observations = SOURCE_EPOCHS
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let observation = observations
        .entry(session.into())
        .or_insert(Observation((marker, file), 0));
    if observation.0 != (marker, file) {
        observation.0 = (marker, file);
        observation.1 = observation.1.saturating_add(1);
    }
    SourceStamp {
        epoch: SourceEpoch::from_observation(observation.1),
        file,
        witness: marker,
    }
}
