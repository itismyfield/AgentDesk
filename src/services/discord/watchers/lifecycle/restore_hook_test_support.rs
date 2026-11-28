use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

type ReaderStart = Arc<Mutex<Option<u64>>>;
static READER_STARTS: LazyLock<Mutex<HashMap<u64, ReaderStart>>> = LazyLock::new(Mutex::default);

pub(in crate::services::discord) struct ReaderStartObservation {
    channel: u64,
    offset: ReaderStart,
}

impl ReaderStartObservation {
    pub(in crate::services::discord) fn install(channel: u64) -> Self {
        let offset = Arc::new(Mutex::new(None));
        let mut starts = READER_STARTS.lock().unwrap();
        assert!(!starts.contains_key(&channel), "one observer per channel");
        starts.insert(channel, offset.clone());
        Self { channel, offset }
    }

    pub(in crate::services::discord) fn offset(&self) -> Option<u64> {
        *self.offset.lock().unwrap()
    }
}

impl Drop for ReaderStartObservation {
    fn drop(&mut self) {
        READER_STARTS.lock().unwrap().remove(&self.channel);
    }
}

pub(super) fn record_reader_start(channel: u64, offset: u64) {
    let Some(target) = READER_STARTS.lock().unwrap().get(&channel).cloned() else {
        return;
    };
    *target.lock().unwrap() = Some(offset);
}
