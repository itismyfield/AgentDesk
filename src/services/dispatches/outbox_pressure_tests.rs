use super::*;
use std::io::Write;
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct Lines(Arc<Mutex<Vec<u8>>>);

impl Write for Lines {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Lines {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

impl Lines {
    fn subscriber(&self) -> impl tracing::Subscriber + Send + Sync + 'static {
        tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .with_writer(self.clone())
            .finish()
    }

    fn warnings(&self) -> Vec<String> {
        String::from_utf8(self.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .filter(|line| line.contains("yielding drain to foreground under pool pressure"))
            .map(str::to_owned)
            .collect()
    }
}

fn busy() -> PressureSnapshot {
    PressureSnapshot {
        pool_size: 15,
        pool_idle: 3,
        pool_max: 18,
        effective_reserve: 6,
    }
}

fn idle() -> PressureSnapshot {
    PressureSnapshot {
        pool_idle: 4,
        ..busy()
    }
}

fn field(line: &str, expected: &str) {
    assert!(
        line.split_whitespace().any(|part| part == expected),
        "missing {expected}: {line}"
    );
}

#[test]
fn pressure_warning_contains_exact_snapshot_used_by_gate() {
    let lines = Lines::default();
    let _subscriber = tracing::subscriber::set_default(lines.subscriber());
    let now = Instant::now();
    let mut pressure = OutboxPressure::new(now);
    assert!(!pressure.yield_tick(idle(), "test-owner", now));
    assert!(lines.warnings().is_empty());
    assert!(pressure.yield_tick(busy(), "test-owner", now));
    assert_eq!(pressure.poll_interval(), MAX_POLL);
    let warnings = lines.warnings();
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("agentdesk::services::dispatches::outbox_queue:"));
    for expected in [
        "pool_size=15",
        "pool_idle=3",
        "pool_max=18",
        "effective_reserve=6",
        "in_flight=12",
        "background_budget=12",
        "last_completed_drain_elapsed_ms=null",
        "window_elapsed_ms=0",
        "skip_count=1",
        "drain_attempt_count=0",
        "claim_count=0",
    ] {
        field(&warnings[0], expected);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn pressure_window_reports_skips_and_drain_attempts_between_warnings() {
    let lines = Lines::default();
    let _subscriber = tracing::subscriber::set_default(lines.subscriber());
    let now = Instant::now();
    let mut pressure = OutboxPressure::new(now);
    assert!(pressure.yield_tick(busy(), "test-owner", now));
    assert!(pressure.yield_tick(busy(), "test-owner", now + Duration::from_secs(5)));
    assert!(!pressure.yield_tick(idle(), "test-owner", now + Duration::from_secs(10)));
    pressure
        .drain(std::future::ready(2), || now + Duration::from_secs(10))
        .await;
    assert_eq!(pressure.poll_interval(), FAST_POLL);
    pressure
        .drain(std::future::ready(0), || now + Duration::from_secs(20))
        .await;
    assert_eq!(pressure.poll_interval(), Duration::from_millis(750));
    assert!(pressure.yield_tick(busy(), "test-owner", now + Duration::from_secs(30)));
    let warnings = lines.warnings();
    assert_eq!(warnings.len(), 2);
    for expected in [
        "skip_count=2",
        "drain_attempt_count=2",
        "claim_count=2",
        "window_elapsed_ms=30000",
        "last_completed_drain_elapsed_ms=10000",
    ] {
        field(&warnings[1], expected);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn pressure_window_uses_null_until_first_completed_drain() {
    let lines = Lines::default();
    let _subscriber = tracing::subscriber::set_default(lines.subscriber());
    let now = Instant::now();
    let mut pressure = OutboxPressure::new(now);
    assert!(pressure.yield_tick(busy(), "test-owner", now));
    field(&lines.warnings()[0], "last_completed_drain_elapsed_ms=null");
    pressure
        .drain(std::future::ready(1), || now + Duration::from_secs(10))
        .await;
    assert!(pressure.yield_tick(busy(), "test-owner", now + Duration::from_secs(29)));
    assert_eq!(lines.warnings().len(), 1);
    assert!(pressure.yield_tick(busy(), "test-owner", now + Duration::from_secs(30)));
    assert_eq!(lines.warnings().len(), 2);
    assert!(pressure.yield_tick(busy(), "test-owner", now + Duration::from_secs(59)));
    assert_eq!(lines.warnings().len(), 2);
    assert!(pressure.yield_tick(busy(), "test-owner", now + Duration::from_secs(60)));
    let warnings = lines.warnings();
    assert_eq!(warnings.len(), 3);
    field(&warnings[1], "last_completed_drain_elapsed_ms=20000");
    field(&warnings[2], "last_completed_drain_elapsed_ms=50000");
    for expected in [
        "skip_count=2",
        "drain_attempt_count=0",
        "claim_count=0",
        "window_elapsed_ms=30000",
    ] {
        field(&warnings[2], expected);
    }
}
