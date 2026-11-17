//! One anonymous `[intake-timing]` line per Discord message: stage offsets from the gateway event,
//! SQLx statement/acquire counts seen under the message's trace span, and the terminal outcome.
//!
//! Observation only. Without the layer installed every call is a no-op. The layer reads only the
//! numeric `acquired_after_secs` of acquire events and, of query events, whether the statement
//! summary is a transaction keyword; SQL text, binds, DSN and message/user identifiers never reach
//! the line. A stage the trace did not see prints `null`, never 0.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tracing::field::{Field, Visit};
use tracing::level_filters::LevelFilter;
use tracing::subscriber::Interest;
use tracing::{Event, Metadata, Span, Subscriber};
use tracing_subscriber::Registry;
use tracing_subscriber::filter::Filtered;
use tracing_subscriber::layer::{Context, Filter, Layer};
use tracing_subscriber::registry::LookupSpan;

pub(crate) const TARGET: &str = "agentdesk::intake_timing";
/// Carries the sample number of the caller's trace over a loopback API request.
pub(crate) const LOOPBACK_HEADER: &str = "x-agentdesk-intake-sample";
const SPAN_NAME: &str = "intake_trace";
const QUERY_TARGET: &str = "sqlx::query";
const ACQUIRE_TARGET: &str = "sqlx::pool::acquire";
/// Traces with no terminal by then are closed as `expired`; bounds memory for routed-away messages.
const STALE_AFTER: Duration = Duration::from_secs(600);
const MAX_OPEN: usize = 256;
const DISCORD_EPOCH_MS: u64 = 1_420_070_400_000;

/// Intake waypoints in path order; `Route` lands on the first free of `route1`/`route2`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stage {
    Recv,
    Route,
    Owner,
    Front,
    Claim,
    Strategy,
    Prep,
    Handoff,
    Applied,
    Settled,
}

const SLOTS: [&str; 11] = [
    "recv", "route1", "owner", "front", "route2", "claim", "strategy", "prep", "handoff",
    "applied", "settled",
];
const IDLE_PATH: [usize; 11] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10];
const BUSY_PATH: [usize; 5] = [0, 1, 2, 3, 10];

impl Stage {
    fn slot(self) -> usize {
        match self {
            Stage::Recv => 0,
            Stage::Route => 1,
            Stage::Owner => 2,
            Stage::Front => 3,
            Stage::Claim => 5,
            Stage::Strategy => 6,
            Stage::Prep => 7,
            Stage::Handoff => 8,
            Stage::Applied => 9,
            Stage::Settled => 10,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// The owner settled a write whose first input was applied.
    Applied,
    /// The owner settled a paste into a busy session.
    BusyPaste,
    /// The owner settled a submission without an applied first-input mark.
    Submitted,
    NotSent,
    Uncertain,
    Cancelled,
    Failed,
    Expired,
}

impl Outcome {
    /// Classifies a terminal error string; a pool timeout or closure also counts an acquire error.
    pub(crate) fn from_error(error: &str) -> (Self, bool) {
        let acquire = [sqlx::Error::PoolTimedOut, sqlx::Error::PoolClosed]
            .iter()
            .any(|pool| error.contains(&pool.to_string()));
        let lower = error.to_ascii_lowercase();
        let outcome = if lower.contains("cancel") {
            Outcome::Cancelled
        } else if lower.contains("uncertain") {
            Outcome::Uncertain
        } else if error.starts_with("NotSent(") {
            Outcome::NotSent
        } else {
            Outcome::Failed
        };
        (outcome, acquire)
    }

    fn label(self) -> &'static str {
        match self {
            Outcome::Applied => "applied",
            Outcome::BusyPaste => "busy_paste",
            Outcome::Submitted => "submitted",
            Outcome::NotSent => "not_sent",
            Outcome::Uncertain => "uncertain",
            Outcome::Cancelled => "cancelled",
            Outcome::Failed => "failed",
            Outcome::Expired => "expired",
        }
    }
}

struct Trace {
    sample: u64,
    message: u64,
    started: Instant,
    gateway_ms: Option<u64>,
    /// Offset in ms plus one; 0 means unobserved.
    marks: [AtomicU64; SLOTS.len()],
    db_ops: AtomicU64,
    tx_controls: AtomicU64,
    acquires: AtomicU64,
    acquire_us: AtomicU64,
    acquire_errors: AtomicU64,
    busy: AtomicBool,
    done: AtomicBool,
}

impl Trace {
    fn mark(&self, stage: Stage) {
        let offset = self.started.elapsed().as_millis() as u64 + 1;
        let first = |slot: usize| {
            self.marks[slot]
                .compare_exchange(0, offset, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        };
        if stage == Stage::Route {
            let _ = first(1) || first(4);
        } else {
            first(stage.slot());
        }
    }

    fn observed(&self, slot: usize) -> Option<u64> {
        self.marks[slot].load(Ordering::Acquire).checked_sub(1)
    }

    fn coverage(&self, outcome: Outcome) -> &'static str {
        let path: &[usize] = if self.busy.load(Ordering::Acquire) {
            &BUSY_PATH
        } else {
            &IDLE_PATH
        };
        let last = path.iter().rposition(|slot| self.observed(*slot).is_some());
        let gap = last.is_some_and(|last| path[..last].iter().any(|s| self.observed(*s).is_none()));
        let whole = path.iter().all(|slot| self.observed(*slot).is_some());
        let needs_whole = matches!(outcome, Outcome::Applied | Outcome::BusyPaste);
        if outcome == Outcome::Expired || gap || (needs_whole && !whole) {
            "partial"
        } else {
            "complete"
        }
    }

    fn line(&self, outcome: Outcome) -> String {
        let mut line = format!(
            "[intake-timing] sample={} outcome={} coverage={}",
            self.sample,
            outcome.label(),
            self.coverage(outcome)
        );
        let _ = write!(line, " gateway_ms={}", opt(self.gateway_ms));
        for (slot, name) in SLOTS.iter().enumerate() {
            let _ = write!(line, " {name}_ms={}", opt(self.observed(slot)));
        }
        let us = self.acquire_us.load(Ordering::Acquire);
        let _ = write!(
            line,
            " db_ops={} tx_controls={} acquires={} acquire_total_ms={}.{} acquire_errors={}",
            self.db_ops.load(Ordering::Acquire),
            self.tx_controls.load(Ordering::Acquire),
            self.acquires.load(Ordering::Acquire),
            us / 1000,
            us % 1000 / 100,
            self.acquire_errors.load(Ordering::Acquire),
        );
        line
    }

    /// Emits the line once; later terminals for the same trace are ignored.
    fn finish(&self, outcome: Outcome) {
        if !self.done.swap(true, Ordering::AcqRel) {
            tracing::info!(target: TARGET, "{}", self.line(outcome));
        }
    }
}

fn opt(value: Option<u64>) -> String {
    value.map_or_else(|| "null".to_owned(), |value| value.to_string())
}

#[derive(Default)]
struct Open {
    by_message: Mutex<HashMap<u64, Arc<Trace>>>,
    samples: AtomicU64,
}

impl Open {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<u64, Arc<Trace>>> {
        self.by_message.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn take(&self, message: u64) -> Option<Arc<Trace>> {
        self.lock().remove(&message)
    }
}

/// Collects the open traces; installed beside the fmt layer by `layer()`.
#[derive(Clone, Default)]
pub(crate) struct IntakeTimingLayer {
    open: Arc<Open>,
}

#[derive(Clone)]
struct TraceRef(Arc<Trace>);

/// Passes trace spans and only those SQLx events emitted under one.
pub(crate) struct InTrace;

pub(crate) fn layer<S>() -> Filtered<IntakeTimingLayer, InTrace, S>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    IntakeTimingLayer::default().with_filter(InTrace)
}

fn is_sqlx(target: &str) -> bool {
    target == QUERY_TARGET || target == ACQUIRE_TARGET
}

fn has_trace<'a, R: LookupSpan<'a>>(
    span: Option<tracing_subscriber::registry::SpanRef<'a, R>>,
) -> bool {
    span.is_some_and(|span| {
        span.scope()
            .any(|s| s.extensions().get::<TraceRef>().is_some())
    })
}

impl<S> Filter<S> for InTrace
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn enabled(&self, meta: &Metadata<'_>, cx: &Context<'_, S>) -> bool {
        if meta.is_span() {
            return meta.target() == TARGET && meta.name() == SPAN_NAME;
        }
        is_sqlx(meta.target()) && has_trace(cx.lookup_current())
    }

    fn callsite_enabled(&self, meta: &'static Metadata<'static>) -> Interest {
        match (meta.is_span(), meta.target()) {
            (true, TARGET) if meta.name() == SPAN_NAME => Interest::always(),
            (false, target) if is_sqlx(target) => Interest::sometimes(),
            _ => Interest::never(),
        }
    }

    fn max_level_hint(&self) -> Option<LevelFilter> {
        Some(LevelFilter::DEBUG)
    }
}

#[derive(Default)]
struct SqlxFields {
    transaction_keyword: bool,
    acquired_after_secs: Option<f64>,
}

impl Visit for SqlxFields {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "summary" {
            let head = value.split_whitespace().next().unwrap_or_default();
            self.transaction_keyword = ["BEGIN", "COMMIT", "ROLLBACK", "START"]
                .iter()
                .any(|keyword| head.eq_ignore_ascii_case(keyword));
        }
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        if field.name() == "acquired_after_secs" {
            self.acquired_after_secs = Some(value);
        }
    }

    fn record_debug(&mut self, _: &Field, _: &dyn std::fmt::Debug) {}
}

impl<S> Layer<S> for IntakeTimingLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let Some(trace) = ctx.event_scope(event).and_then(|scope| {
            scope
                .into_iter()
                .find_map(|span| span.extensions().get::<TraceRef>().map(|t| t.0.clone()))
        }) else {
            return;
        };
        let mut fields = SqlxFields::default();
        event.record(&mut fields);
        if event.metadata().target() == QUERY_TARGET {
            trace.db_ops.fetch_add(1, Ordering::AcqRel);
            if fields.transaction_keyword {
                trace.tx_controls.fetch_add(1, Ordering::AcqRel);
            }
        } else if let Some(secs) = fields.acquired_after_secs {
            trace.acquires.fetch_add(1, Ordering::AcqRel);
            trace
                .acquire_us
                .fetch_add((secs * 1_000_000.0) as u64, Ordering::AcqRel);
        }
    }
}

fn installed() -> Option<Arc<Open>> {
    tracing::dispatcher::get_default(|dispatch| {
        dispatch
            .downcast_ref::<IntakeTimingLayer>()
            .map(|layer| layer.open.clone())
    })
}

/// A span carrying `trace`; `root` detaches it from the caller's spans for a thread hop.
fn span_with(trace: Arc<Trace>, root: bool) -> Span {
    let span = if root {
        tracing::debug_span!(target: TARGET, parent: None, SPAN_NAME)
    } else {
        tracing::debug_span!(target: TARGET, SPAN_NAME)
    };
    span.with_subscriber(|(id, dispatch)| {
        if let Some(span) = dispatch
            .downcast_ref::<Registry>()
            .and_then(|registry| registry.span(id))
        {
            span.extensions_mut().insert(TraceRef(trace));
        }
    });
    span
}

fn current() -> Option<Arc<Trace>> {
    Span::current()
        .with_subscriber(|(id, dispatch)| {
            let registry = dispatch.downcast_ref::<Registry>()?;
            let span = registry.span(id)?;
            span.scope()
                .find_map(|s| s.extensions().get::<TraceRef>().map(|t| t.0.clone()))
        })
        .flatten()
}

/// Opens the trace of a message that passed the gateway guards, timed from `event_started`, and
/// returns the span its intake runs under. A repeated message keeps its first trace.
pub(crate) fn begin(message: u64, event_started: Instant) -> Span {
    let Some(open) = installed() else {
        return Span::none();
    };
    let created = (message >> 22) + DISCORD_EPOCH_MS;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
        .saturating_sub(event_started.elapsed().as_millis() as u64);
    let (trace, expired) = {
        let mut traces = open.lock();
        let mut expired: Vec<Arc<Trace>> = Vec::new();
        traces.retain(|_, trace| {
            let keep = trace.started.elapsed() < STALE_AFTER;
            if !keep {
                expired.push(trace.clone());
            }
            keep
        });
        if traces.len() >= MAX_OPEN
            && !traces.contains_key(&message)
            && let Some(oldest) = traces.values().min_by_key(|t| t.started).map(|t| t.message)
        {
            expired.extend(traces.remove(&oldest));
        }
        let trace = traces
            .entry(message)
            .or_insert_with(|| {
                let trace = Trace {
                    sample: open.samples.fetch_add(1, Ordering::AcqRel) + 1,
                    message,
                    started: event_started,
                    gateway_ms: now.checked_sub(created),
                    marks: Default::default(),
                    db_ops: AtomicU64::new(0),
                    tx_controls: AtomicU64::new(0),
                    acquires: AtomicU64::new(0),
                    acquire_us: AtomicU64::new(0),
                    acquire_errors: AtomicU64::new(0),
                    busy: AtomicBool::new(false),
                    done: AtomicBool::new(false),
                };
                trace.mark(Stage::Recv);
                Arc::new(trace)
            })
            .clone();
        (trace, expired)
    };
    for stale in expired {
        stale.finish(Outcome::Expired);
    }
    span_with(trace, false)
}

/// The span of `message`'s open trace for work another task runs on its behalf.
pub(crate) fn span_for(message: u64) -> Span {
    let trace = installed().and_then(|open| open.lock().get(&message).cloned());
    trace.map_or_else(Span::none, |trace| span_with(trace, false))
}

/// A root span on the current trace, to enter on the other side of a thread hop.
pub(crate) fn carry() -> Span {
    current().map_or_else(Span::none, |trace| span_with(trace, true))
}

pub(crate) fn mark(stage: Stage) {
    if let Some(trace) = current() {
        trace.mark(stage);
    }
}

pub(crate) fn mark_message(message: u64, stage: Stage) {
    if let Some(trace) = installed().and_then(|open| open.lock().get(&message).cloned()) {
        trace.mark(stage);
    }
}

/// Marks the current trace's input as a paste into a busy session.
pub(crate) fn note_busy() {
    if let Some(trace) = current() {
        trace.busy.store(true, Ordering::Release);
    }
}

fn close(trace: &Trace, open: Option<Arc<Open>>, outcome: Outcome, acquire_error: bool) {
    if acquire_error && !trace.done.load(Ordering::Acquire) {
        trace.acquire_errors.fetch_add(1, Ordering::AcqRel);
    }
    if let Some(open) = open {
        let mut traces = open.lock();
        if traces
            .get(&trace.message)
            .is_some_and(|t| std::ptr::eq(&**t, trace))
        {
            traces.remove(&trace.message);
        }
    }
    trace.finish(outcome);
}

/// Ends the current trace once the owner settled its write; `refused` names a settled write that
/// did not submit.
pub(crate) fn settle(refused: Option<Outcome>) {
    if let Some(trace) = current() {
        trace.mark(Stage::Settled);
        let outcome = match (refused, trace.busy.load(Ordering::Acquire)) {
            (Some(outcome), _) => outcome,
            (None, true) => Outcome::BusyPaste,
            (None, false) if trace.observed(Stage::Applied.slot()).is_some() => Outcome::Applied,
            (None, false) => Outcome::Submitted,
        };
        close(&trace, installed(), outcome, false);
    }
}

/// Ends the current trace when the owner kept the original unsent for `reason`.
pub(crate) fn not_sent(reason: &str) {
    if let Some(trace) = current() {
        let outcome = match Outcome::from_error(reason).0 {
            Outcome::Cancelled => Outcome::Cancelled,
            _ => Outcome::NotSent,
        };
        close(&trace, installed(), outcome, false);
    }
}

/// Ends `message`'s trace with the result its owner front handoff returned, unless a settle did.
pub(crate) fn finish_message(message: u64, result: &Result<(), String>) {
    let Some(open) = installed() else { return };
    let Some(trace) = open.take(message) else {
        return;
    };
    match result {
        Ok(()) => close(&trace, None, Outcome::Submitted, false),
        Err(error) => {
            let (outcome, acquire) = Outcome::from_error(error);
            close(&trace, None, outcome, acquire);
        }
    }
}

/// The loopback header naming the current trace, when there is one.
pub(crate) fn loopback_header() -> Option<(&'static str, String)> {
    current().map(|trace| (LOOPBACK_HEADER, trace.sample.to_string()))
}

/// Runs a loopback API request under the trace its header names, so the handler's DB work counts.
pub(crate) async fn api_trace_middleware(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use tracing::Instrument;
    let sample = request
        .headers()
        .get(LOOPBACK_HEADER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    let trace = sample.zip(installed()).and_then(|(sample, open)| {
        open.lock()
            .values()
            .find(|trace| trace.sample == sample)
            .cloned()
    });
    match trace {
        Some(trace) => next.run(request).instrument(span_with(trace, false)).await,
        None => next.run(request).await,
    }
}

#[cfg(test)]
#[path = "intake_timing_tests.rs"]
pub(crate) mod tests;
