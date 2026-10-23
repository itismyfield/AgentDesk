//! The raw files of one `health_amplification_profile` run, written once and read back for
//! every summary, so the judge only ever sees what was recorded.

use std::collections::BTreeMap;
use std::path::Path;

use super::amplification_verdict_tests::{
    Acquire, Answer, Attempt, Endpoint, Lane, Outcome, Peak, PolicyAttempt, PolicyOutcome, Profile,
    Resources, RunData, Sample,
};

mod raw_cases_tests;

pub(super) const NONE: &str = "-";

pub(super) fn opt(value: Option<impl ToString>) -> String {
    value.map_or_else(|| NONE.to_string(), |value| value.to_string())
}

/// Tabs and line breaks would split a record, so free text keeps neither.
fn flat(text: &str) -> String {
    text.replace(['\t', '\n', '\r'], " ")
}

pub(super) fn write_run(out: &Path, run: &RunData) {
    let lines = |rows: Vec<String>| rows.join("\n") + "\n";
    let conditions = run
        .conditions
        .iter()
        .map(|(key, value)| format!("{key}\t{value}"));
    std::fs::write(out.join("conditions.tsv"), lines(conditions.collect())).unwrap();
    let mut rows = vec![
        "endpoint\tseq\tmeasured\tscheduled_us\tsent_us\tlast_byte_us\toutcome\tstatus\tparsed\tobservation\thostname\treasons"
            .to_string(),
    ];
    for attempt in &run.attempts {
        let (outcome, answer) = match &attempt.outcome {
            Outcome::Unsent => ("unsent".to_string(), None),
            Outcome::Timeout => ("timeout".to_string(), None),
            Outcome::Failed(kind) => (format!("failed:{kind}"), None),
            Outcome::Answered(answer) => ("answered".to_string(), Some(answer)),
        };
        rows.push(format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{outcome}\t{}\t{}\t{}\t{}\t{}",
            attempt.endpoint.name(),
            attempt.seq,
            attempt.measured,
            attempt.scheduled,
            opt(attempt.sent),
            opt(attempt.last_byte),
            opt(answer.map(|a| a.status)),
            opt(answer.map(|a| a.parsed)),
            answer.map_or(NONE, |a| a.observation_status.as_str()),
            opt(answer.map(|a| a.node_hostname)),
            answer.map_or(NONE.to_string(), |a| a.reasons.join(",")),
        ));
    }
    std::fs::write(out.join("requests.tsv"), lines(rows)).unwrap();
    let ticks = run
        .ticks
        .iter()
        .map(|(slot, woke)| format!("{slot}\t{woke}"));
    std::fs::write(out.join("ticks.tsv"), lines(ticks.collect())).unwrap();
    let r = &run.resources;
    let mut rows = vec![
        format!("pool_max\t{}", r.pool_max),
        format!("pg_statements\t{}", r.pg_statements),
    ];
    for (point, sample) in [("start", r.start), ("end", r.end)] {
        for field in Sample::FIELDS {
            rows.push(format!("{point}.{field}\t{}", opt(sample.field(field))));
        }
    }
    if run.profile() == Some(Profile::S2) {
        rows.push(format!("peak.proxy_open\t{}", opt(r.peak.proxy_open)));
        rows.push(format!("peak.pool_size\t{}", opt(r.peak.pool_size)));
        rows.push(format!("peak.beyond_pool\t{}", opt(r.peak.beyond_pool)));
        rows.push(format!("policy_statements\t{}", opt(r.policy_statements)));
        write_policy(out, run);
    }
    std::fs::write(out.join("resources.tsv"), lines(rows)).unwrap();
}

fn write_policy(out: &Path, run: &RunData) {
    let lines = |rows: Vec<String>| rows.join("\n") + "\n";
    let mut rows = vec!["lane\tseq\tmeasured\tscheduled_us\tsent_us\tend_us\toutcome".to_string()];
    for hook in &run.policy {
        let outcome = match &hook.outcome {
            PolicyOutcome::Unsent => "unsent".to_string(),
            PolicyOutcome::Timeout => "timeout".to_string(),
            PolicyOutcome::Read => "read".to_string(),
            PolicyOutcome::Failed(error) => format!("failed:{}", flat(error)),
            PolicyOutcome::Mismatch(detail) => format!("mismatch:{}", flat(detail)),
        };
        rows.push(format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{outcome}",
            hook.lane.name(),
            hook.seq,
            hook.measured,
            hook.scheduled,
            opt(hook.sent),
            opt(hook.end),
        ));
    }
    std::fs::write(out.join("policy.tsv"), lines(rows)).unwrap();
    let mut rows = vec!["measured\twaited_us".to_string()];
    for acquire in &run.acquires {
        rows.push(format!("{}\t{}", acquire.measured, opt(acquire.waited)));
    }
    std::fs::write(out.join("acquire.tsv"), lines(rows)).unwrap();
}

fn key_values(path: &Path) -> BTreeMap<String, String> {
    std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()))
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

fn parse<T: std::str::FromStr>(field: &str) -> Option<T> {
    (field != NONE).then(|| field.parse().ok()).flatten()
}

/// Reads a run back; with `runner` it also requires the runner's evidence that the test passed
/// and that no foreign build overlapped it. A count that is missing or unreadable stays None.
pub(super) fn load_run(dir: &Path, runner: bool) -> RunData {
    let name = dir.file_name().map_or_else(
        || dir.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    );
    let mut run = RunData {
        name,
        conditions: key_values(&dir.join("conditions.tsv")),
        ..RunData::default()
    };
    let requests = std::fs::read_to_string(dir.join("requests.tsv")).expect("requests.tsv");
    for line in requests.lines().skip(1) {
        let fields: Vec<&str> = line.split('\t').collect();
        let attempt = (fields.len() == 12)
            .then(|| attempt_from(&fields))
            .flatten();
        match attempt {
            Some(attempt) => run.attempts.push(attempt),
            None => run.invalid.push("requests_unreadable".to_string()),
        }
    }
    let ticks = std::fs::read_to_string(dir.join("ticks.tsv")).expect("ticks.tsv");
    for line in ticks.lines() {
        match line
            .split_once('\t')
            .and_then(|(slot, woke)| Some((slot.parse().ok()?, woke.parse().ok()?)))
        {
            Some(tick) => run.ticks.push(tick),
            None => run.invalid.push("ticks_unreadable".to_string()),
        }
    }
    let resources = key_values(&dir.join("resources.tsv"));
    let number = |key: &str| resources.get(key).and_then(|value| parse::<usize>(value));
    let point = |point: &str| Sample {
        threads: number(&format!("{point}.threads")),
        pool_size: number(&format!("{point}.pool_size")).and_then(|size| size.try_into().ok()),
        pool_idle: number(&format!("{point}.pool_idle")),
        proxy_accepted: number(&format!("{point}.proxy_accepted")),
        proxy_open: number(&format!("{point}.proxy_open")),
    };
    run.resources = Resources {
        start: point("start"),
        end: point("end"),
        pool_max: number("pool_max").unwrap_or_default() as u32,
        pg_statements: number("pg_statements").unwrap_or_default(),
        peak: Peak {
            proxy_open: number("peak.proxy_open"),
            pool_size: number("peak.pool_size").and_then(|size| size.try_into().ok()),
            beyond_pool: number("peak.beyond_pool"),
        },
        policy_statements: number("policy_statements"),
    };
    if let Ok(policy) = std::fs::read_to_string(dir.join("policy.tsv")) {
        for line in policy.lines().skip(1) {
            let fields: Vec<&str> = line.split('\t').collect();
            match (fields.len() == 7).then(|| hook_from(&fields)).flatten() {
                Some(hook) => run.policy.push(hook),
                None => run.invalid.push("policy_unreadable".to_string()),
            }
        }
    }
    if let Ok(acquires) = std::fs::read_to_string(dir.join("acquire.tsv")) {
        for line in acquires.lines().skip(1) {
            let acquire = line.split_once('\t').and_then(|(measured, waited)| {
                let waited = match waited {
                    NONE => None,
                    waited => Some(waited.parse().ok()?),
                };
                Some(Acquire {
                    measured: measured.parse().ok()?,
                    waited,
                })
            });
            match acquire {
                Some(acquire) => run.acquires.push(acquire),
                None => run.invalid.push("acquires_unreadable".to_string()),
            }
        }
    }
    if runner {
        run.invalid.extend(runner_evidence(dir));
    }
    run.invalid.dedup();
    run
}

fn attempt_from(fields: &[&str]) -> Option<Attempt> {
    let outcome = match fields[6] {
        "unsent" => Outcome::Unsent,
        "timeout" => Outcome::Timeout,
        "answered" => Outcome::Answered(Answer {
            status: parse(fields[7])?,
            parsed: parse(fields[8])?,
            observation_status: fields[9].to_string(),
            node_hostname: parse(fields[10])?,
            reasons: fields[11]
                .split(',')
                .filter(|reason| !reason.is_empty())
                .map(str::to_string)
                .collect(),
        }),
        failed => Outcome::Failed(failed.strip_prefix("failed:")?.to_string()),
    };
    Some(Attempt {
        endpoint: Endpoint::parse(fields[0])?,
        seq: fields[1].parse().ok()?,
        measured: fields[2].parse().ok()?,
        scheduled: fields[3].parse().ok()?,
        sent: parse(fields[4]),
        last_byte: parse(fields[5]),
        outcome,
    })
}

fn hook_from(fields: &[&str]) -> Option<PolicyAttempt> {
    let outcome = match fields[6] {
        "unsent" => PolicyOutcome::Unsent,
        "timeout" => PolicyOutcome::Timeout,
        "read" => PolicyOutcome::Read,
        other => match other.split_once(':')? {
            ("failed", error) => PolicyOutcome::Failed(error.to_string()),
            ("mismatch", detail) => PolicyOutcome::Mismatch(detail.to_string()),
            _ => return None,
        },
    };
    Some(PolicyAttempt {
        lane: Lane::parse(fields[0])?,
        seq: fields[1].parse().ok()?,
        measured: fields[2].parse().ok()?,
        scheduled: fields[3].parse().ok()?,
        sent: parse(fields[4]),
        end: parse(fields[5]),
        outcome,
    })
}

/// `test.log` is this run's captured test output; `monitor.tsv` has one line per 5 s:
/// epoch, 1-minute load, the count of foreign build processes, and their names.
fn runner_evidence(dir: &Path) -> Vec<String> {
    let mut invalid = Vec::new();
    let log = std::fs::read_to_string(dir.join("test.log")).unwrap_or_default();
    if !log.contains("test result: ok. 1 passed") {
        invalid.push("test_not_passed".to_string());
    }
    let monitor = std::fs::read_to_string(dir.join("monitor.tsv")).unwrap_or_default();
    let counts: Vec<Option<usize>> = monitor
        .lines()
        .map(|line| line.split('\t').nth(2).and_then(|count| count.parse().ok()))
        .collect();
    if counts.is_empty() || counts.contains(&None) {
        invalid.push("monitor_missing".to_string());
    } else if counts.iter().any(|count| *count != Some(0)) {
        invalid.push("foreign_build_overlap".to_string());
    }
    invalid
}
