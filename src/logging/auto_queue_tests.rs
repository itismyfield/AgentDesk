use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use tracing::field::{Field, Visit};
use tracing::{Event, Id, Subscriber, span};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;

use crate::domain::auto_queue_log::AutoQueueLogContext;
use crate::logging::auto_queue::auto_queue_trace_span;

#[derive(Clone, Debug)]
struct SpanRecord {
    name: String,
    target: String,
    level: tracing::Level,
    schema: BTreeSet<String>,
    fields: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default)]
struct Capture {
    spans: BTreeMap<u64, SpanRecord>,
    events: Vec<(Vec<String>, BTreeMap<String, String>)>,
    lifetime: Vec<(String, String)>,
}

struct Fields<'a>(&'a mut BTreeMap<String, String>);

impl Visit for Fields<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0
            .insert(field.name().to_string(), format!("{value:?}"));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_string(), value.to_string());
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.0
            .insert(field.name().to_string(), format!("i64:{value}"));
    }
}

#[derive(Clone)]
struct CaptureLayer(Arc<Mutex<Capture>>);

impl<S: Subscriber + for<'lookup> LookupSpan<'lookup>> Layer<S> for CaptureLayer {
    fn on_new_span(&self, attrs: &span::Attributes<'_>, id: &Id, context: Context<'_, S>) {
        let metadata = context.span(id).expect("new span registered").metadata();
        let mut fields = BTreeMap::new();
        attrs.record(&mut Fields(&mut fields));
        self.0.lock().unwrap().spans.insert(
            id.into_u64(),
            SpanRecord {
                name: metadata.name().to_string(),
                target: metadata.target().to_string(),
                level: *metadata.level(),
                schema: metadata
                    .fields()
                    .iter()
                    .map(|field| field.name().to_string())
                    .collect(),
                fields,
            },
        );
    }

    fn on_record(&self, id: &Id, values: &span::Record<'_>, context: Context<'_, S>) {
        assert!(context.span(id).is_some());
        let mut capture = self.0.lock().unwrap();
        values.record(&mut Fields(
            &mut capture.spans.get_mut(&id.into_u64()).unwrap().fields,
        ));
    }

    fn on_event(&self, event: &Event<'_>, context: Context<'_, S>) {
        let scope = context
            .event_scope(event)
            .map(|scope| {
                scope
                    .from_root()
                    .map(|span| span.name().to_string())
                    .collect()
            })
            .unwrap_or_default();
        let mut fields = BTreeMap::new();
        event.record(&mut Fields(&mut fields));
        self.0.lock().unwrap().events.push((scope, fields));
    }

    fn on_enter(&self, id: &Id, context: Context<'_, S>) {
        let name = context.span(id).unwrap().name().to_string();
        self.0
            .lock()
            .unwrap()
            .lifetime
            .push(("enter".to_string(), name));
    }

    fn on_exit(&self, id: &Id, context: Context<'_, S>) {
        let name = context.span(id).unwrap().name().to_string();
        self.0
            .lock()
            .unwrap()
            .lifetime
            .push(("exit".to_string(), name));
    }
}

fn capture(body: impl FnOnce()) -> Capture {
    crate::logging::test_capture::pin_callsite_interest();
    let result = Arc::new(Mutex::new(Capture::default()));
    tracing::subscriber::with_default(
        tracing_subscriber::registry().with(CaptureLayer(result.clone())),
        body,
    );
    let snapshot = result.lock().unwrap().clone();
    snapshot
}

fn auto_span(capture: &Capture) -> &SpanRecord {
    capture
        .spans
        .values()
        .find(|span| span.name == "auto_queue")
        .expect("auto queue span")
}

fn schema() -> BTreeSet<String> {
    [
        "action",
        "run_id",
        "entry_id",
        "card_id",
        "dispatch_id",
        "thread_group",
        "batch_phase",
        "slot_index",
        "agent_id",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

#[test]
fn auto_queue_span_preserves_schema_values_and_explicit_enter_lifetime() {
    let run = String::from(" run ");
    let context = AutoQueueLogContext::new()
        .run(&run)
        .entry("entry")
        .card("card")
        .dispatch("dispatch")
        .agent("agent")
        .thread_group(-2)
        .batch_phase(4)
        .slot_index(0);
    assert!(std::ptr::eq(context.run_id.unwrap(), run.as_str()));
    let observed = capture(|| {
        let parent = tracing::info_span!("parent");
        let parent_guard = parent.enter();
        let span = auto_queue_trace_span("synthetic", &context);
        assert_eq!(
            tracing::Span::current().metadata().unwrap().name(),
            "parent"
        );
        tracing::info!("before enter");
        {
            let guard = span.enter();
            tracing::info!("inside span");
            drop(guard);
        }
        tracing::info!("after exit");
        drop(parent_guard);
    });
    let span = auto_span(&observed);
    assert_eq!(span.target, "agentdesk::services::auto_queue");
    assert_eq!(span.level, tracing::Level::INFO);
    assert_eq!(span.schema, schema());
    let expected = [
        ("action", "synthetic"),
        ("run_id", " run "),
        ("entry_id", "entry"),
        ("card_id", "card"),
        ("dispatch_id", "dispatch"),
        ("thread_group", "i64:-2"),
        ("batch_phase", "i64:4"),
        ("slot_index", "i64:0"),
        ("agent_id", "agent"),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_string(), value.to_string()))
    .collect();
    assert_eq!(span.fields, expected);
    assert_eq!(
        observed
            .events
            .iter()
            .map(|event| event.0.clone())
            .collect::<Vec<_>>(),
        vec![vec!["parent"], vec!["parent", "auto_queue"], vec!["parent"]]
    );
    assert_eq!(
        observed.lifetime,
        vec![
            ("enter".into(), "parent".into()),
            ("enter".into(), "auto_queue".into()),
            ("exit".into(), "auto_queue".into()),
            ("exit".into(), "parent".into())
        ]
    );
}

#[test]
fn auto_queue_optional_fields_remain_declared_and_dispatch_filter_preserves_borrows() {
    for dispatch in [None, Some(""), Some(" \t\n")] {
        let context = AutoQueueLogContext::default()
            .maybe_dispatch(dispatch)
            .slot_index(9)
            .maybe_slot_index(None);
        assert!(context.dispatch_id.is_none());
        let observed = capture(|| {
            auto_queue_trace_span("empty", &context);
        });
        assert_eq!(auto_span(&observed).schema, schema());
        assert_eq!(
            auto_span(&observed).fields,
            BTreeMap::from([("action".to_string(), "empty".to_string())])
        );
        assert!(observed.lifetime.is_empty());
    }
    let dispatch = String::from(" dispatch \t");
    let context = AutoQueueLogContext::new()
        .maybe_dispatch(Some(&dispatch))
        .maybe_slot_index(Some(-1));
    assert!(std::ptr::eq(
        context.dispatch_id.unwrap(),
        dispatch.as_str()
    ));
    let observed = capture(|| {
        auto_queue_trace_span("present", &context);
    });
    assert_eq!(
        auto_span(&observed).fields.get("dispatch_id").unwrap(),
        &dispatch
    );
    assert_eq!(
        auto_span(&observed).fields.get("slot_index").unwrap(),
        "i64:-1"
    );
    let direct = AutoQueueLogContext::new().dispatch(" \t");
    assert_eq!(direct.dispatch_id, Some(" \t"));
}

#[test]
fn auto_queue_macro_evaluates_context_once_then_enters_through_event_arguments() {
    let evaluations = Cell::new(0);
    let observed = capture(|| {
        let parent = tracing::info_span!("parent");
        let guard = parent.enter();
        crate::auto_queue_log!(
            info,
            "macro",
            {
                evaluations.set(evaluations.get() + 1);
                assert_eq!(
                    tracing::Span::current().metadata().unwrap().name(),
                    "parent"
                );
                AutoQueueLogContext::new().run("macro-run")
            },
            "{}",
            {
                assert_eq!(
                    tracing::Span::current().metadata().unwrap().name(),
                    "auto_queue"
                );
                "macro event"
            }
        );
        assert_eq!(
            tracing::Span::current().metadata().unwrap().name(),
            "parent"
        );
        tracing::info!("parent event");
        drop(guard);
        assert!(tracing::Span::current().is_none());
    });
    assert_eq!(evaluations.get(), 1);
    assert_eq!(
        auto_span(&observed).fields.get("run_id").unwrap(),
        "macro-run"
    );
    assert_eq!(observed.events[0].0, vec!["parent", "auto_queue"]);
    assert_eq!(observed.events[0].1.get("message").unwrap(), "macro event");
    assert_eq!(observed.events[1].0, vec!["parent"]);
    assert_eq!(
        observed.lifetime,
        vec![
            ("enter".into(), "parent".into()),
            ("enter".into(), "auto_queue".into()),
            ("exit".into(), "auto_queue".into()),
            ("exit".into(), "parent".into())
        ]
    );
}

#[test]
fn auto_queue_compatibility_paths_reexport_the_same_items() {
    let legacy: crate::services::auto_queue::AutoQueueLogContext<'_> =
        AutoQueueLogContext::new().run("same-item");
    let canonical: AutoQueueLogContext<'_> = legacy;
    assert_eq!(canonical.run_id, Some("same-item"));
    let old: fn(&'static str, &AutoQueueLogContext<'_>) -> tracing::Span =
        crate::services::auto_queue::auto_queue_trace_span;
    let new: fn(&'static str, &AutoQueueLogContext<'_>) -> tracing::Span = auto_queue_trace_span;
    assert!(std::ptr::fn_addr_eq(old, new));
}
