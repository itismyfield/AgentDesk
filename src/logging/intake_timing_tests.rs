use super::*;
use crate::test_support::wait::{WAIT_CAP, join_within};
use tracing_subscriber::layer::SubscriberExt as _;

#[derive(Clone, Default)]
pub(crate) struct Lines(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Lines {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Lines {
    type Writer = Lines;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

impl Lines {
    pub(crate) fn timing(&self) -> Vec<String> {
        String::from_utf8(self.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .filter(|line| line.contains("[intake-timing]"))
            .map(str::to_owned)
            .collect()
    }
}

/// The dcserver stack: an INFO fmt layer beside the intake timing layer (or without it).
pub(crate) fn dispatch(lines: &Lines, timing: bool) -> tracing::Dispatch {
    crate::logging::test_capture::pin_callsite_interest();
    let fmt = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(lines.clone())
        .with_filter(LevelFilter::INFO);
    let registry = tracing_subscriber::registry().with(fmt);
    match timing {
        true => tracing::Dispatch::new(registry.with(layer())),
        false => tracing::Dispatch::new(registry),
    }
}

pub(crate) fn field<'a>(line: &'a str, name: &str) -> &'a str {
    let key = format!(" {name}=");
    let start = line
        .find(&key)
        .unwrap_or_else(|| panic!("{name} in {line}"))
        + key.len();
    line[start..].split(' ').next().unwrap()
}

const MESSAGE: u64 = 1_439_000_000_000_000_007;

#[test]
fn one_line_per_message_follows_the_trace_across_tasks_and_threads() {
    let lines = Lines::default();
    let dispatch = dispatch(&lines, true);
    let _default = tracing::dispatcher::set_default(&dispatch);
    begin(MESSAGE, Instant::now()).in_scope(|| {
        mark(Stage::Route);
        mark(Stage::Owner);
    });
    // The owner drive runs the front in another task, found again by message.
    mark_message(MESSAGE, Stage::Front);
    span_for(MESSAGE).in_scope(|| {
        for stage in [Stage::Route, Stage::Claim, Stage::Strategy, Stage::Prep] {
            mark(stage);
        }
        let carried = carry();
        let dispatch = dispatch.clone();
        let driver = std::thread::spawn(move || {
            tracing::dispatcher::with_default(&dispatch, || {
                let _entered = carried.entered();
                tracing::event!(target: "sqlx::query", tracing::Level::DEBUG,
                    summary = "SELECT secret_column FROM", db.statement = "SELECT secret_column FROM t WHERE v = 'bind-secret'",
                    elapsed_secs = 0.004);
                tracing::event!(target: "sqlx::query", tracing::Level::DEBUG,
                    summary = "BEGIN", db.statement = "", elapsed_secs = 0.001);
                tracing::event!(target: "sqlx::pool::acquire", tracing::Level::DEBUG,
                    acquired_after_secs = 0.0125, "acquired connection");
                mark(Stage::Handoff);
                mark(Stage::Applied);
                settle(None);
            })
        });
        join_within(driver, WAIT_CAP);
    });
    finish_message(MESSAGE, &Ok(()));
    span_for(MESSAGE).in_scope(|| settle(None));

    let found = lines.timing();
    assert_eq!(found.len(), 1, "{found:?}");
    let line = &found[0];
    assert_eq!(field(line, "outcome"), "applied", "{line}");
    assert_eq!(field(line, "coverage"), "complete", "{line}");
    for name in SLOTS {
        assert_ne!(field(line, &format!("{name}_ms")), "null", "{name}: {line}");
    }
    assert_eq!(field(line, "db_ops"), "2", "{line}");
    assert_eq!(field(line, "tx_controls"), "1", "{line}");
    assert_eq!(field(line, "acquires"), "1", "{line}");
    assert_eq!(field(line, "acquire_total_ms"), "12.5", "{line}");
    for secret in ["secret", "SELECT", &MESSAGE.to_string()] {
        assert!(!line.contains(secret), "{secret} leaked: {line}");
    }
}

#[test]
fn unseen_stages_print_null_and_partial_coverage_and_no_layer_prints_nothing() {
    let lines = Lines::default();
    tracing::dispatcher::with_default(&dispatch(&lines, true), || {
        begin(MESSAGE, Instant::now()).in_scope(|| {
            mark(Stage::Route);
            settle(None);
        });
    });
    let found = lines.timing();
    assert_eq!(found.len(), 1, "{found:?}");
    let line = &found[0];
    assert_eq!(field(line, "outcome"), "submitted", "{line}");
    assert_eq!(field(line, "coverage"), "partial", "{line}");
    for name in ["owner_ms", "front_ms", "strategy_ms", "applied_ms"] {
        assert_eq!(field(line, name), "null", "{name}: {line}");
    }
    assert_ne!(field(line, "settled_ms"), "null", "{line}");

    let silent = Lines::default();
    tracing::dispatcher::with_default(&dispatch(&silent, false), || {
        begin(MESSAGE, Instant::now()).in_scope(|| settle(None));
        finish_message(MESSAGE, &Err("NotSent(\"x\")".into()));
    });
    assert!(silent.timing().is_empty());
}
