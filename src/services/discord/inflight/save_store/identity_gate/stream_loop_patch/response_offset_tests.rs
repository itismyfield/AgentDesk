use super::*;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
use tracing::{Event, Level, Subscriber, field};
use tracing_subscriber::{Layer, layer::Context, prelude::*};

#[derive(Default)]
struct Fields(BTreeMap<String, String>);

impl field::Visit for Fields {
    fn record_str(&mut self, field: &field::Field, value: &str) {
        self.0.insert(field.name().to_string(), value.to_string());
    }

    fn record_debug(&mut self, field: &field::Field, value: &dyn std::fmt::Debug) {
        self.0
            .insert(field.name().to_string(), format!("{value:?}"));
    }
}

struct OffsetLogs(Arc<Mutex<Vec<(Level, Fields)>>>);

impl<S: Subscriber> Layer<S> for OffsetLogs {
    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        let mut fields = Fields::default();
        event.record(&mut fields);
        if fields.0.contains_key("invariant") {
            self.0
                .lock()
                .unwrap()
                .push((*event.metadata().level(), fields));
        }
    }
}

fn stream_save_logs(offset: usize) -> Vec<(Level, Fields)> {
    let root = tempfile::tempdir().unwrap();
    let mut baseline = InflightTurnState::new(
        ProviderKind::Claude,
        849_101,
        None,
        0,
        37,
        0,
        String::new(),
        None,
        None,
        None,
        None,
        0,
    );
    baseline.full_response = "é".to_string();
    baseline.response_sent_offset = 2;
    save_inflight_state_in_root(root.path(), &baseline).unwrap();
    let expected = InflightTurnIdentity::from_state(&baseline);
    let mut local = baseline.clone();
    local.response_sent_offset = offset;
    let logs = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry().with(OffsetLogs(Arc::clone(&logs)));
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        tracing::subscriber::with_default(subscriber, || {
            save_stream_tick_state_preserving_current_message_races_in_root(
                root.path(),
                &mut baseline,
                &mut local,
                &expected,
                0,
                0,
                "turn_bridge::stream_tick::dirty_flush_o_cut",
            )
        })
    }));
    match offset {
        0 => assert_eq!(outcome.unwrap(), GuardedSaveOutcome::AuthorityPinned),
        _ if cfg!(debug_assertions) => assert!(outcome.is_err()),
        _ => assert_eq!(outcome.unwrap(), GuardedSaveOutcome::Saved),
    }
    std::mem::take(&mut *logs.lock().unwrap())
}

fn assert_fields(fields: Fields, invariant: &str, offset: usize, in_bounds: bool) {
    let fields = fields.0;
    assert_eq!(fields["invariant"], invariant);
    assert_eq!(fields["channel_id"], "849101");
    assert_eq!(fields["turn_id"], "discord:849101:37");
    let response_offset: Value = serde_json::from_str(&fields["response_offset"]).unwrap();
    let snapshot = json!({
        "response_sent_offset": 2, "full_response_len": 2, "in_bounds": true,
    });
    assert_eq!(
        response_offset,
        json!({
            "entrypoint": "turn_bridge::stream_tick::dirty_flush_o_cut",
            "merge_branch": "before_eq_durable",
            "finalizer_turn_id": "37",
            "response_sent_offset": offset,
            "full_response_len": 2,
            "candidate_in_bounds": in_bounds,
            "before": snapshot,
            "local": {
                "response_sent_offset": offset, "full_response_len": 2, "in_bounds": in_bounds,
            },
            "durable": snapshot,
        })
    );
}

#[test]
fn stream_save_logs_out_of_bounds_offset_inputs() {
    let logs = stream_save_logs(3);
    assert_eq!(logs.len(), if cfg!(debug_assertions) { 1 } else { 2 });
    for (level, fields) in logs {
        assert_eq!(level, Level::ERROR);
        assert_fields(fields, "response_sent_offset_in_bounds", 3, false);
    }
}

#[test]
fn stream_save_logs_monotonic_warn_offset_inputs() {
    let mut logs = stream_save_logs(0);
    assert_eq!(logs.len(), 1);
    let (level, fields) = logs.pop().unwrap();
    assert_eq!(level, Level::WARN);
    assert_fields(fields, "response_sent_offset_monotonic", 0, true);
}
