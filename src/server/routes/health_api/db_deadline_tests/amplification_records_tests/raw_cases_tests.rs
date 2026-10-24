//! Raw run files with one resource count missing or broken: reading them back keeps that count
//! unobserved, so the judge refuses the run instead of counting a zero.

use super::super::amplification_verdict_tests::{self as verdict, Profile};
use super::*;

/// A minimal health-only run whose resources are whole and steady.
fn whole_run() -> RunData {
    let sample = Sample {
        threads: Some(11),
        pool_size: Some(8),
        pool_idle: Some(8),
        proxy_accepted: Some(8),
        proxy_open: Some(8),
    };
    RunData {
        conditions: [
            ("profile", Profile::S1.name()),
            ("seed", "7"),
            ("rps_per_endpoint", "10"),
            ("warmup_secs", "2"),
            ("measure_secs", "3"),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect(),
        resources: Resources {
            start: sample,
            end: sample,
            pool_max: 8,
            pg_statements: 10,
            ..Resources::default()
        },
        ..RunData::default()
    }
}

fn unobserved(dir: &Path) -> Vec<String> {
    verdict::summarize(&load_run(dir, false))
        .invalid
        .into_iter()
        .filter(|reason| reason.starts_with("resource_unobserved"))
        .collect()
}

#[test]
fn amplification_records_keep_a_missing_or_broken_resource_count_unobserved() {
    let dir = tempfile::tempdir().unwrap();
    write_run(dir.path(), &whole_run());
    assert_eq!(unobserved(dir.path()), Vec::<String>::new());
    let whole = std::fs::read_to_string(dir.path().join("resources.tsv")).unwrap();
    for point in ["start", "end"] {
        for field in [
            "pool_idle",
            "proxy_accepted",
            "proxy_open",
            "pool_size",
            "threads",
        ] {
            let key = format!("{point}.{field}\t");
            for broken in [String::new(), format!("{key}x\n"), format!("{key}-\n")] {
                let resources: String = whole
                    .lines()
                    .map(|line| match line.starts_with(&key) {
                        true => broken.clone(),
                        false => format!("{line}\n"),
                    })
                    .collect();
                std::fs::write(dir.path().join("resources.tsv"), resources).unwrap();
                assert_eq!(
                    unobserved(dir.path()),
                    [format!("resource_unobserved:{point}.{field}")],
                    "{broken:?}"
                );
            }
        }
    }
}
