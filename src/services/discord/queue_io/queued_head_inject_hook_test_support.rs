use super::AttemptRow;
use poise::serenity_prelude::{ChannelId, MessageId};
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

#[derive(Default)]
struct Observation {
    offers: Vec<MessageId>,
    rows: Vec<AttemptRow>,
}

static OBSERVATIONS: LazyLock<Mutex<HashMap<u64, Observation>>> = LazyLock::new(Mutex::default);

pub(in crate::services::discord) struct Probe(u64);

impl Probe {
    pub(in crate::services::discord) fn install(channel: ChannelId) -> Self {
        let channel = channel.get();
        let previous = OBSERVATIONS
            .lock()
            .unwrap()
            .insert(channel, Observation::default());
        assert!(previous.is_none());
        Self(channel)
    }

    pub(in crate::services::discord) fn offers(&self) -> Vec<MessageId> {
        OBSERVATIONS.lock().unwrap()[&self.0].offers.clone()
    }

    pub(in crate::services::discord) fn rows(&self) -> Vec<AttemptRow> {
        OBSERVATIONS.lock().unwrap()[&self.0].rows.clone()
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        OBSERVATIONS.lock().unwrap().remove(&self.0);
    }
}

pub(super) fn offer(channel: ChannelId, message: MessageId) {
    if let Some(observation) = OBSERVATIONS.lock().unwrap().get_mut(&channel.get()) {
        observation.offers.push(message);
    }
}

pub(super) fn attempt(row: &AttemptRow) {
    if let Some(observation) = OBSERVATIONS.lock().unwrap().get_mut(&row.channel_id) {
        observation.rows.push(row.clone());
    }
}
