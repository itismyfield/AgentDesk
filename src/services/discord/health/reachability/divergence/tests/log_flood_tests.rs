use super::*;
use std::{
    io::Write,
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl Write for Logs {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Logs {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }

    fn count(&self, message: &str) -> usize {
        self.text()
            .lines()
            .filter(|line| line.contains(message))
            .count()
    }

    fn capture(&self) -> tracing::subscriber::DefaultGuard {
        crate::logging::test_capture::pin_callsite_interest();
        let writer = self.clone();
        tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .without_time()
                .with_ansi(false)
                .with_max_level(tracing::Level::TRACE)
                .with_writer(move || writer.clone())
                .finish(),
        )
    }
}

const DIVERGENCE: &str = "disagree by file identity";

#[tokio::test(start_paused = true)]
async fn divergence_log_flood_is_bounded_and_identity_changes_emit_immediately() {
    let dir = tempfile::tempdir().unwrap();
    let paths = ["row", "registry", "changed"].map(|name| dir.path().join(name));
    for path in &paths {
        std::fs::write(path, "same size").unwrap();
    }
    let [row, registry, changed] = paths.each_ref().map(|path| path.to_str().unwrap());
    let channel = 799_300_002;
    let logs = Logs::default();
    let _capture = logs.capture();
    for _ in 0..120 {
        assert_eq!(
            observe_row_coordinate_divergence("codex", channel, Some(row), Some(registry)),
            RowCoordinateDivergence::Diverged,
        );
        tokio::time::advance(Duration::from_millis(500)).await;
    }
    assert_eq!(logs.count(DIVERGENCE), 1, "{}", logs.text());
    assert!(logs.text().contains("WARN"));
    for (provider, channel, row, registry) in [
        ("codex", channel + 1, row, registry),
        ("claude", channel, row, registry),
        ("codex", channel, row, changed),
        ("codex", channel, changed, registry),
        ("codex", channel, row, registry),
    ] {
        assert_eq!(
            observe_row_coordinate_divergence(provider, channel, Some(row), Some(registry)),
            RowCoordinateDivergence::Diverged,
        );
    }
    assert_eq!(logs.count(DIVERGENCE), 6, "{}", logs.text());
    assert_eq!(
        observe_row_coordinate_divergence("codex", channel, Some(row), Some(row)),
        RowCoordinateDivergence::SameFile,
    );
    assert_eq!(
        observe_row_coordinate_divergence("codex", channel, Some(row), Some(registry)),
        RowCoordinateDivergence::Diverged,
    );
    assert_eq!(logs.count(DIVERGENCE), 7, "{}", logs.text());
}

#[tokio::test(start_paused = true)]
async fn divergence_summaries_preserve_identity_aliases_missing_rows_and_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let row = dir.path().join("row");
    let registry = dir.path().join("registry");
    let alias = dir.path().join("row-alias");
    let missing = dir.path().join("missing");
    std::fs::write(&row, "a").unwrap();
    std::fs::write(&registry, "b").unwrap();
    std::os::unix::fs::symlink(&row, &alias).unwrap();
    let channel = 799_300_004;
    let observe = |row: &std::path::Path, registry: &std::path::Path| {
        observe_row_coordinate_divergence("codex", channel, row.to_str(), registry.to_str())
    };
    let logs = Logs::default();
    let _capture = logs.capture();
    for _ in 0..120 {
        assert_eq!(observe(&row, &registry), RowCoordinateDivergence::Diverged);
        tokio::time::advance(Duration::from_millis(500)).await;
    }
    tokio::time::advance(Duration::from_millis(539_500)).await;
    assert_eq!(
        observe(&alias, &registry),
        RowCoordinateDivergence::Diverged
    );
    assert_eq!(logs.count(DIVERGENCE), 1, "{}", logs.text());
    tokio::time::advance(Duration::from_millis(500)).await;
    assert_eq!(observe(&row, &registry), RowCoordinateDivergence::Diverged);
    assert_eq!(logs.count(DIVERGENCE), 2, "{}", logs.text());
    assert!(
        logs.text()
            .contains("suppressed 120 times since previous log"),
        "{}",
        logs.text()
    );
    assert_eq!(
        observe(&alias, &registry),
        RowCoordinateDivergence::Diverged
    );
    tokio::time::advance(Duration::from_secs(600)).await;
    assert_eq!(observe(&row, &registry), RowCoordinateDivergence::Diverged);
    assert_eq!(logs.count(DIVERGENCE), 3, "{}", logs.text());
    assert!(
        logs.text()
            .contains("suppressed 1 times since previous log"),
        "{}",
        logs.text()
    );
    for _ in 0..120 {
        assert_eq!(
            observe(&missing, &registry),
            RowCoordinateDivergence::RowPathUnresolvableWhileRegistryLive
        );
    }
    assert_eq!(logs.count(DIVERGENCE), 4, "{}", logs.text());
    assert_eq!(observe(&row, &row), RowCoordinateDivergence::SameFile);
    assert_eq!(
        logs.count("reachability_row_coordinate_divergence_cleared"),
        1,
        "{}",
        logs.text()
    );
    assert_eq!(observe(&row, &registry), RowCoordinateDivergence::Diverged);
    assert_eq!(logs.count(DIVERGENCE), 5, "{}", logs.text());
    assert_eq!(observe(&row, &missing), RowCoordinateDivergence::Unknown);
    assert_eq!(
        logs.count("reachability_row_coordinate_divergence_cleared"),
        2,
        "{}",
        logs.text()
    );
    assert_eq!(observe(&row, &registry), RowCoordinateDivergence::Diverged);
    assert_eq!(logs.count(DIVERGENCE), 6, "{}", logs.text());
}

#[test]
fn concurrent_divergence_observers_emit_one_first_line_without_changing_results() {
    use crate::test_support::wait::{WAIT_CAP, join_within};
    let channel = 799_300_006;
    let dir = tempfile::tempdir().unwrap();
    let row = dir.path().join("row");
    let registry = dir.path().join("registry");
    std::fs::write(&row, "a").unwrap();
    std::fs::write(&registry, "b").unwrap();
    let logs = Logs::default();
    let threads = (0..16)
        .map(|_| {
            let (logs, row, registry) = (logs.clone(), row.clone(), registry.clone());
            std::thread::spawn(move || {
                let _capture = logs.capture();
                for _ in 0..120 {
                    assert_eq!(
                        observe_row_coordinate_divergence(
                            "codex",
                            channel,
                            row.to_str(),
                            registry.to_str()
                        ),
                        RowCoordinateDivergence::Diverged,
                    );
                }
            })
        })
        .collect::<Vec<_>>();
    for thread in threads {
        join_within(thread, WAIT_CAP);
    }
    assert_eq!(logs.count(DIVERGENCE), 1, "{}", logs.text());
}

#[tokio::test(start_paused = true)]
async fn long_turn_pair_stays_within_ten_lines_over_thirty_two_minutes() {
    use crate::services::discord::live_bridge;
    use crate::services::provider::ProviderKind;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let channel = 799_320_001;
    let _original = live_bridge::log_flood_tests::registered_original(channel).await;
    let dir = tempfile::tempdir().unwrap();
    let row = dir.path().join("row");
    let registry = dir.path().join("registry");
    std::fs::write(&row, "a").unwrap();
    std::fs::write(&registry, "b").unwrap();
    let logs = Logs::default();
    let _capture = logs.capture();
    for tick in 0..=3840 {
        assert!(live_bridge::try_recovery(&ProviderKind::Codex, channel).is_err());
        if tick % 20 == 0 {
            assert_eq!(
                observe_row_coordinate_divergence(
                    "codex",
                    channel,
                    row.to_str(),
                    registry.to_str()
                ),
                RowCoordinateDivergence::Diverged,
            );
        }
        if tick < 3840 {
            tokio::time::advance(Duration::from_millis(500)).await;
        }
    }
    let counts = [
        logs.count("live_original_bridge_deferred"),
        logs.count(DIVERGENCE),
    ];
    assert!(
        counts.iter().sum::<usize>() <= 10,
        "counts={counts:?}\n{}",
        logs.text()
    );
    assert_eq!(counts, [4, 4], "{}", logs.text());
}

#[tokio::test(start_paused = true)]
async fn summary_lines_report_actual_elapsed_since_previous_log() {
    use crate::services::discord::live_bridge;
    use crate::services::provider::ProviderKind;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let channel = 799_320_002;
    let _original = live_bridge::log_flood_tests::registered_original(channel).await;
    let dir = tempfile::tempdir().unwrap();
    let row = dir.path().join("row");
    let registry = dir.path().join("registry");
    std::fs::write(&row, "a").unwrap();
    std::fs::write(&registry, "b").unwrap();
    let logs = Logs::default();
    let _capture = logs.capture();
    for delay in [0, 600, 675] {
        tokio::time::advance(Duration::from_secs(delay)).await;
        assert!(live_bridge::try_recovery(&ProviderKind::Codex, channel).is_err());
        assert_eq!(
            observe_row_coordinate_divergence("codex", channel, row.to_str(), registry.to_str()),
            RowCoordinateDivergence::Diverged,
        );
    }
    for marker in ["live_original_bridge_deferred", DIVERGENCE] {
        let periods = logs
            .text()
            .lines()
            .filter(|line| line.contains(marker))
            .filter_map(|line| {
                line.split_whitespace().find_map(|field| {
                    field
                        .strip_prefix("since_previous_log_secs=")
                        .map(|value| value.parse::<u64>().unwrap())
                })
            })
            .collect::<Vec<_>>();
        assert_eq!(periods, [0, 600, 675], "{marker}\n{}", logs.text());
    }
}

#[test]
fn core_summary_periods_follow_previous_emission_not_episode_age() {
    use crate::services::discord::idle_relay_drift::{DriftState, decide_drift_warn};
    let start = std::time::Instant::now();
    let mut state = DriftState::new(start);
    let decisions = [0, 300, 675].map(|second| {
        decide_drift_warn(
            &mut state,
            start + Duration::from_secs(second),
            Duration::from_secs(300),
        )
    });
    assert!(decisions.iter().all(|decision| decision.emit));
    assert_eq!(
        decisions
            .each_ref()
            .map(|decision| decision.since_previous_log_secs),
        [0, 300, 375]
    );
    assert_eq!(
        decisions.each_ref().map(|decision| decision.drift_age_secs),
        [0, 300, 675]
    );
}
