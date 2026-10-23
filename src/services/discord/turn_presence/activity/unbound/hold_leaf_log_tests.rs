use super::superseded;
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct LogSink(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogSink {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn superseded_logs_power_of_two_info_and_complete_debug_per_reason() {
    const CHILD: &str = "ADK_SUPERSEDED_LOG_CHILD";
    let Ok(level) = std::env::var(CHILD) else {
        let test = concat!(
            module_path!(),
            "::superseded_logs_power_of_two_info_and_complete_debug_per_reason"
        );
        let test = test
            .strip_prefix(concat!(env!("CARGO_CRATE_NAME"), "::"))
            .unwrap();
        for level in ["INFO", "DEBUG"] {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", test, "--nocapture", "--test-threads=1"])
                .env(CHILD, level)
                .env_remove("TMUX")
                .env_remove("TMUX_PANE")
                .env_remove("AGENTDESK_CONFIG")
                .output()
                .unwrap();
            let logs = format!(
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            eprintln!("{level} child: {logs}");
            assert!(output.status.success(), "{level} child failed: {logs}");
            assert!(
                logs.contains("1 passed; 0 failed; 0 ignored;"),
                "child must execute exactly one test: {logs}"
            );
        }
        return;
    };

    let sink = LogSink::default();
    let writer = sink.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_target(false)
        .with_max_level(match level.as_str() {
            "INFO" => tracing::Level::INFO,
            "DEBUG" => tracing::Level::DEBUG,
            _ => panic!("unexpected child level {level}"),
        })
        .with_writer(move || writer.clone())
        .finish();
    crate::logging::test_capture::pin_callsite_interest();
    tracing::subscriber::with_default(subscriber, || {
        for _ in 0..1_000 {
            superseded("providers_busy");
        }
        superseded("registry_busy");
    });
    let logs = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
    let events: Vec<_> = logs
        .lines()
        .filter(|line| line.contains("unbound observation superseded"))
        .collect();
    let repeated: Vec<_> = events
        .iter()
        .filter(|line| line.contains("reason=\"providers_busy\""))
        .collect();
    let count = |line: &&&str| -> u64 {
        line.split("superseded_count=")
            .nth(1)
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .parse()
            .unwrap()
    };
    let info: Vec<_> = repeated
        .iter()
        .filter(|line| line.trim_start().starts_with("INFO"))
        .map(count)
        .collect();
    assert!(
        info.contains(&2),
        "continued supersessions must retain an INFO trend at count 2"
    );
    assert_eq!(
        info.len(),
        10,
        "1000 supersessions must emit 10 INFO events"
    );
    assert_eq!(info, [1, 2, 4, 8, 16, 32, 64, 128, 256, 512]);
    if level == "DEBUG" {
        assert_eq!(
            repeated.len(),
            1_000,
            "DEBUG must retain every supersession"
        );
        assert_eq!(
            repeated.iter().map(count).collect::<Vec<_>>(),
            (1..=1_000).collect::<Vec<_>>()
        );
        assert_eq!(
            repeated
                .iter()
                .filter(|line| line.trim_start().starts_with("DEBUG"))
                .count(),
            990
        );
    } else {
        assert_eq!(repeated.len(), 10, "INFO must exclude unsampled repeats");
    }
    let other: Vec<_> = events
        .iter()
        .filter(|line| line.contains("reason=\"registry_busy\""))
        .collect();
    assert_eq!(other.len(), 1, "another reason must keep its first warning");
    assert!(other[0].trim_start().starts_with("INFO"));
    assert_eq!(count(&other[0]), 1);
    eprintln!(
        "SUPERSEDED_LOGS level={level} calls=1000 info={} events={} other_info=1",
        info.len(),
        repeated.len()
    );
}
