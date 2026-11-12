#[path = "sources.rs"]
mod sources;
pub(crate) use sources::{config, github, services, utils};
#[path = "capture.rs"]
mod capture;
#[path = "../../src/server/routes/github/issue_duplicate.rs"]
pub(crate) mod issue_duplicate;
#[path = "report.rs"]
mod report;
#[path = "transport.rs"]
pub(crate) mod transport;

use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

const MODEL: &str = "gemini-decision-1";
const TEMPLATE: &str = "issue_duplicate_v1";
const SENTINEL: &str = "acceptance-loopback-only-secret-sentinel";
type Result<T> = std::result::Result<T, &'static str>;

#[derive(Deserialize)]
struct Samples {
    schema_version: u32,
    issue_sets: BTreeMap<String, Vec<issue_duplicate::OpenIssue>>,
    samples: Vec<Sample>,
}

#[derive(Deserialize)]
struct Sample {
    sample_id: String,
    expected: String,
    key: String,
    title: String,
    background: String,
    issue_set: String,
}

pub(crate) fn main() {
    // Parse and handle the absent key before creating threads, files, or clients.
    let result = options().and_then(|(out, self_test)| {
        let key = if self_test {
            SENTINEL.to_string()
        } else {
            match std::env::var("GEMINI_API_KEY") {
                Ok(key) if !key.trim().is_empty() => key.trim().to_owned(),
                _ => {
                    println!("SKIPPED_NO_KEY");
                    return Ok(());
                }
            }
        };
        let out = out.ok_or("missing_--out_DIR")?;
        utils::redact::register_known_secret(&key);
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| "runtime_unavailable")?
            .block_on(run(&out, key, self_test))
    });
    if let Err(reason) = result {
        eprintln!("HARNESS_FAILED reason={reason}");
        std::process::exit(1);
    }
}

fn options() -> Result<(Option<PathBuf>, bool)> {
    let mut args = std::env::args_os().skip(1);
    let mut out = None;
    let mut self_test = false;
    while let Some(arg) = args.next() {
        if arg == "--out" && out.is_none() {
            out = Some(PathBuf::from(args.next().ok_or("missing_out_directory")?));
        } else if arg == "--self-test" && !self_test {
            self_test = true;
        } else {
            return Err("usage_--out_DIR_[--self-test]");
        }
    }
    Ok((out, self_test))
}

fn samples() -> Result<Samples> {
    let data: Samples = serde_json::from_str(include_str!(
        "../../tests/fixtures/decision_gate/acceptance_samples.json"
    ))
    .map_err(|_| "invalid_samples")?;
    let mut ids = std::collections::BTreeSet::new();
    let (mut duplicates, mut new, mut large) = (0, 0, 0);
    for sample in &data.samples {
        let issues = data
            .issue_sets
            .get(&sample.issue_set)
            .ok_or("unknown_issue_set")?;
        if !ids.insert(&sample.sample_id) || sample.title.trim().is_empty() {
            return Err("invalid_sample_identity");
        }
        let numbers: std::collections::BTreeSet<_> = issues.iter().map(|i| i.number).collect();
        if numbers.len() != issues.len()
            || issues.is_empty()
            || issues.len() > 254
            || issues
                .iter()
                .any(|i| i.number <= 0 || i.title.trim().is_empty())
        {
            return Err("invalid_candidates");
        }
        match sample.expected.as_str() {
            "duplicate"
                if issues
                    .iter()
                    .any(|i| sample.key == format!("issue_{}", i.number)) =>
            {
                duplicates += 1
            }
            "new" if sample.key == "none" => new += 1,
            _ => return Err("invalid_label"),
        }
        large += usize::from(issues.len() == 254);
    }
    if data.schema_version != 1 || duplicates < 10 || new < 10 || large < 10 {
        return Err("insufficient_samples");
    }
    Ok(data)
}

async fn run(out: &Path, key: String, self_test: bool) -> Result<()> {
    let data = samples()?;
    if out.exists()
        && std::fs::read_dir(out)
            .map_err(|_| "out_directory_failed")?
            .next()
            .is_some()
    {
        return Err("output_directory_not_empty");
    }
    std::fs::create_dir_all(out).map_err(|_| "out_directory_failed")?;
    let (relay, task) = transport::start(key.clone(), self_test).await?;
    let result = async {
        capture::capture(out, &relay, self_test).await?;
        let gate =
            services::decision_gate::DecisionGate::new(&key, self_test.then(|| relay.endpoint()));
        let mut rows = Vec::new();
        let mut distributions = Vec::new();
        for sample in &data.samples {
            relay.clear().await;
            let mut fetch_ms = 0.0;
            let started = Instant::now();
            let outcome = issue_duplicate::check_with_fetch(
                Some(&gate),
                "itismyfield/AgentDesk",
                &sample.title,
                &sample.background,
                false,
                || async {
                    let fetch_start = Instant::now();
                    let issues = data.issue_sets[&sample.issue_set].clone();
                    fetch_ms = fetch_start.elapsed().as_secs_f64() * 1000.0;
                    Ok(issues)
                },
            )
            .await;
            let observation = gate
                .take_observation()
                .ok_or("missing_decision_observation")?;
            let row = report::row(
                sample,
                &outcome,
                &Some(observation.record),
                fetch_ms,
                &observation.observation,
                observation.deadline_ms,
                started.elapsed().as_secs_f64() * 1000.0,
            )?;
            distributions
                .push(json!({"sample_id": sample.sample_id, "probabilities": row.probabilities}));
            rows.push(row);
        }
        report::write(out, &rows, &distributions, self_test)?;
        if self_test {
            report::verify_self_test(out, &rows, relay.request_count())?;
            println!(
                "LOOPBACK_E2E_OK samples={} requests={}",
                rows.len(),
                relay.request_count()
            );
        } else {
            println!(
                "ACCEPTANCE_MEASURED samples={} decision_attempts={} raw_requests=1",
                rows.len(),
                rows.len()
            );
        }
        Ok(())
    }
    .await;
    task.abort();
    let _ = task.await;
    result
}

fn write_json(out: &Path, name: &str, value: &Value) -> Result<()> {
    let text = serde_json::to_string_pretty(value).map_err(|_| "json_serialization_failed")?;
    write_safe(out, name, &text)
}

fn write_safe(out: &Path, name: &str, text: &str) -> Result<()> {
    check_safe(text)?;
    std::fs::write(out.join(name), text).map_err(|_| "output_write_failed")
}

fn check_safe(text: &str) -> Result<()> {
    if utils::redact::contains_registered_secret(text)
        || utils::redact::contains_sensitive_header_prefix(text)
        || text.to_ascii_lowercase().contains("x-goog-api-key")
    {
        return Err("unsafe_output_rejected");
    }
    Ok(())
}
