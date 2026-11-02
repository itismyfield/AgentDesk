use super::JOURNAL;
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    future::Future,
    sync::{LazyLock, Mutex},
};

/// Queue now, then acknowledge durability wherever the caller can await safely.
pub(crate) fn record(record: Value) -> impl Future<Output = ()> + Send + 'static {
    let queued = JOURNAL.get().and_then(|journal| {
        journal
            .0
            .enqueue(record)
            .map(|received| (&journal.0, received))
    });
    async move {
        if let Some((writer, received)) = queued {
            writer.wait_for_ack(received).await;
        }
    }
}

pub(crate) fn try_record(record: Value) {
    if let Some(journal) = JOURNAL.get() {
        journal.0.try_append(record);
    }
}

#[derive(Serialize)]
struct CreatedBucket {
    channel: u64,
    bucket_start_ms: i64,
    first_at_ms: i64,
    last_at_ms: i64,
    confirmed: u64,
    latest_id: u64,
}

static O_CREATED: LazyLock<Mutex<VecDeque<CreatedBucket>>> = LazyLock::new(Mutex::default);

pub(crate) fn record_o_created(channel: u64, id: u64, started_at_ms: Option<i64>) {
    let created_at_ms = ((id >> 22) + 1_420_070_400_000) as i64;
    // A nonce echo is not a new create; ambiguous clock ordering only loses denominator evidence.
    if started_at_ms.is_some_and(|started| created_at_ms <= started) {
        return;
    }
    let bucket_start_ms = created_at_ms.div_euclid(60_000) * 60_000;
    let mut buckets = O_CREATED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(bucket) = buckets
        .iter_mut()
        .find(|bucket| bucket.channel == channel && bucket.bucket_start_ms == bucket_start_ms)
    {
        if id > bucket.latest_id {
            bucket.latest_id = id;
            bucket.last_at_ms = created_at_ms;
            bucket.confirmed = bucket.confirmed.saturating_add(1);
        }
    } else {
        buckets.push_back(CreatedBucket {
            channel,
            bucket_start_ms,
            first_at_ms: created_at_ms,
            last_at_ms: created_at_ms,
            confirmed: 1,
            latest_id: id,
        });
        // Denominator loss is allowed; a bounded snapshot cannot certify absent Legacy traffic.
        if buckets.len() > 256 {
            buckets.pop_front();
        }
    }
}

pub(super) fn snapshot() -> Value {
    json!(
        *O_CREATED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    )
}
