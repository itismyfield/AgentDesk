//! `health_amplification_profile`: public and detail health under a fixed open-loop load against
//! an isolated PostgreSQL, with a 1 s tick probe on the same two-worker server runtime.
//! It runs only by exact name; the raw samples and the judge's summary go to `ADK_HEALTH_AMP_OUT`.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::amplification_verdict_tests::{
    self as verdict, Answer, Attempt, Comparison, Endpoint, EndpointSummary, MetricComparison,
    Outcome, Plan, Resources, RunData, RunSummary, SEC, Sample, Stat,
};
use super::resource_regression_tests::process_threads;
use super::{Db, reasons, registry_with_mailboxes, serve};

const OUT_ENV: &str = "ADK_HEALTH_AMP_OUT";
/// `base_dir,...;candidate_dir,...` judges finished runs instead of measuring.
const COMPARE_ENV: &str = "ADK_HEALTH_AMP_COMPARE";
/// Shortened windows for a smoke run; the judge then refuses its sample counts.
const WARMUP_ENV: &str = "ADK_HEALTH_AMP_WARMUP_SECS";
const MEASURE_ENV: &str = "ADK_HEALTH_AMP_MEASURE_SECS";
const SEED: u64 = 7236;
const RPS: u64 = 10;
const WARMUP_SECS: u64 = 60;
const MEASURE_SECS: u64 = 360;
const SERVER_WORKERS: usize = 2;
const GENERATOR_WORKERS: usize = 4;
const POOL_MAX: u32 = 8;
const MAILBOXES: [u64; 3] = [7_236_100_001, 7_236_100_002, 7_236_100_003];
const CLIENT_TIMEOUT: Duration = Duration::from_secs(10);
const MANIFEST: &str = r#"{"generated_at":"2026-10-23T00:00:00Z","repo_head":"0123456789abcdef0123456789abcdef01234567","latest_postgres_migration":"0104_example.sql","repo_dirty":"false"}"#;
/// The harness as compiled, so a comparison can tell two binaries ran the same one.
const HARNESS_SOURCES: [&str; 4] = [
    include_str!("amplification_profile_tests.rs"),
    include_str!("amplification_verdict_tests.rs"),
    include_str!("resource_regression_tests.rs"),
    include_str!("../db_deadline_tests.rs"),
];

#[test]
#[ignore = "a 7-minute load measurement against a pg_throwaway cluster: run by exact name with ADK_HEALTH_AMP_OUT"]
fn health_amplification_profile() {
    let out = PathBuf::from(
        std::env::var_os(OUT_ENV).unwrap_or_else(|| panic!("{OUT_ENV} must name the run dir")),
    );
    std::fs::create_dir_all(&out).expect("create the run dir");
    match std::env::var(COMPARE_ENV) {
        Ok(spec) => compare_runs(&spec, &out),
        Err(_) => measure(&out),
    }
}

fn measure(out: &Path) {
    let base = crate::db::postgres::postgres_test_database_url_base();
    if let Err(refusal) = verdict::isolated_pg_base(base.as_deref()) {
        panic!("refusing to load this PostgreSQL: {refusal}");
    }
    let plan = Plan {
        seed: SEED,
        rps: RPS,
        warmup_secs: env_secs(WARMUP_ENV, WARMUP_SECS),
        measure_secs: env_secs(MEASURE_ENV, MEASURE_SECS),
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    let runtime_dir = crate::config::runtime_root()
        .expect("the guard sets a runtime root")
        .join("runtime");
    std::fs::create_dir_all(&runtime_dir).expect("create runtime dir");
    std::fs::write(runtime_dir.join("release-source.json"), MANIFEST).expect("write manifest");

    let infra = runtime("amp-infra", 2);
    let db = infra.block_on(async {
        let db = Db::start(Duration::ZERO, POOL_MAX).await;
        db.seed_blocks().await;
        db
    });
    let pg_version: String = infra
        .block_on(sqlx::query_scalar("SHOW server_version").fetch_one(&db.seed))
        .expect("read the server version");
    let server = runtime("amp-server", SERVER_WORKERS);
    let addr = server.block_on(async {
        let mut engine_config = crate::config::Config::default();
        engine_config.policies.hot_reload = false;
        let engine =
            crate::engine::PolicyEngine::new_with_pg(&engine_config, Some(db.pool.clone()))
                .expect("policy engine with PostgreSQL");
        let registry = registry_with_mailboxes(&MAILBOXES).await;
        let tx = crate::server::ws::new_broadcast();
        let buf = crate::server::ws::spawn_batch_flusher(tx.clone());
        serve(crate::server::routes::api_router_with_pg(
            engine,
            crate::config::Config::default(),
            tx,
            buf,
            Some(registry),
            Some(db.pool.clone()),
        ))
        .await
    });

    let origin = Instant::now() + Duration::from_secs(1);
    let wakes = Arc::new(Mutex::new(Vec::new()));
    server.spawn(tick_probe(origin, plan.tick_slots(), Arc::clone(&wakes)));
    let generator = runtime("amp-load", GENERATOR_WORKERS);
    let load = generator.spawn(drive_load(addr, origin, plan));
    sleep_until(origin + Duration::from_micros(plan.warmup_end()));
    let start = sample(&db);
    db.proxy.reset_executions();
    sleep_until(origin + Duration::from_micros(plan.end()));
    let pg_statements = db.proxy.executions().len();
    let end = sample(&db);
    let attempts = generator
        .block_on(load)
        .expect("the load generator finishes");
    let ticks = wakes.lock().unwrap().clone();
    server.block_on(db.pool.close());
    server.shutdown_timeout(Duration::from_secs(5));
    generator.shutdown_timeout(Duration::from_secs(5));
    infra.block_on(db.finish());

    let conditions = conditions(plan, &pg_version);
    let resources = Resources {
        start,
        end,
        pool_max: POOL_MAX,
        pg_statements,
    };
    write_run(out, &conditions, &attempts, &ticks, &resources);
    let summary = verdict::summarize(&load_run(out, false));
    let rendered = render_run(&summary);
    std::fs::write(out.join("summary.txt"), &rendered).expect("write summary");
    println!("{rendered}");
}

fn env_secs(name: &str, default: u64) -> u64 {
    std::env::var(name).map_or(default, |value| {
        value.parse().unwrap_or_else(|_| panic!("{name}={value}"))
    })
}

fn runtime(name: &str, workers: usize) -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .thread_name(name)
        .enable_all()
        .build()
        .expect("runtime")
}

/// Waits for a scheduled instant of the measurement itself.
fn sleep_until(deadline: Instant) {
    std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
}

fn micros_since(origin: Instant) -> u64 {
    Instant::now().saturating_duration_since(origin).as_micros() as u64
}

fn sample(db: &Db) -> Sample {
    Sample {
        threads: std::panic::catch_unwind(process_threads).ok(),
        pool_size: db.pool.size(),
        pool_idle: db.pool.num_idle(),
        proxy_accepted: db.proxy.accepted(),
        proxy_open: db.proxy.open(),
    }
}

fn conditions(plan: Plan, pg_version: &str) -> BTreeMap<String, String> {
    let mut harness = Sha256::new();
    for source in HARNESS_SOURCES {
        harness.update(source.as_bytes());
    }
    let binary = std::fs::read(std::env::current_exe().expect("test binary path"))
        .expect("read the test binary");
    [
        ("seed", plan.seed.to_string()),
        ("rps_per_endpoint", plan.rps.to_string()),
        ("warmup_secs", plan.warmup_secs.to_string()),
        ("measure_secs", plan.measure_secs.to_string()),
        ("server_workers", SERVER_WORKERS.to_string()),
        ("generator_workers", GENERATOR_WORKERS.to_string()),
        ("client_timeout_ms", CLIENT_TIMEOUT.as_millis().to_string()),
        ("pool_max", POOL_MAX.to_string()),
        ("mailboxes", MAILBOXES.len().to_string()),
        ("policy_engine", "new_with_pg".to_string()),
        ("pg_version", pg_version.to_string()),
        ("debug_assertions", cfg!(debug_assertions).to_string()),
        ("harness_sha256", format!("{:x}", harness.finalize())),
        (
            verdict::BINARY_KEY,
            format!("{:x}", Sha256::digest(&binary)),
        ),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_string(), value))
    .collect()
}

/// Wakes at `origin + k s`; a wake that comes after later slots were due skips them, so they stay
/// missing rather than becoming zero-delay catch-up samples.
async fn tick_probe(origin: Instant, slots: u64, wakes: Arc<Mutex<Vec<(u64, u64)>>>) {
    let mut slot = 0;
    while slot < slots {
        tokio::time::sleep_until((origin + Duration::from_secs(slot)).into()).await;
        let woke = micros_since(origin);
        wakes.lock().unwrap().push((slot, woke));
        slot = slot.max(woke / SEC) + 1;
    }
}

async fn drive_load(addr: SocketAddr, origin: Instant, plan: Plan) -> Vec<Attempt> {
    let lanes = Endpoint::ALL.map(|endpoint| tokio::spawn(dispatch(addr, origin, plan, endpoint)));
    let mut attempts = Vec::new();
    for lane in lanes {
        attempts.extend(lane.await.expect("dispatcher"));
    }
    attempts
}

/// Open loop: a send waits only for its own scheduled time, never for an earlier answer; one
/// still unsent when the window closes is recorded unsent.
async fn dispatch(
    addr: SocketAddr,
    origin: Instant,
    plan: Plan,
    endpoint: Endpoint,
) -> Vec<Attempt> {
    let mut attempts = Vec::new();
    let mut in_flight = Vec::new();
    for (seq, scheduled) in plan.schedule(endpoint).into_iter().enumerate() {
        tokio::time::sleep_until((origin + Duration::from_micros(scheduled)).into()).await;
        let attempt = Attempt {
            endpoint,
            seq,
            measured: scheduled >= plan.warmup_end(),
            scheduled,
            sent: None,
            last_byte: None,
            outcome: Outcome::Unsent,
        };
        if micros_since(origin) >= plan.end() {
            attempts.push(attempt);
        } else {
            in_flight.push(tokio::spawn(exchange(addr, origin, attempt)));
        }
    }
    for request in in_flight {
        attempts.push(request.await.expect("request task"));
    }
    attempts
}

/// One GET on a new loopback connection, bounded by the client timeout from the actual send.
async fn exchange(addr: SocketAddr, origin: Instant, mut attempt: Attempt) -> Attempt {
    attempt.sent = Some(micros_since(origin));
    let path = attempt.endpoint.path();
    let request = format!("GET {path} HTTP/1.1\r\nhost: {addr}\r\nconnection: close\r\n\r\n");
    let read = tokio::time::timeout(CLIENT_TIMEOUT, async {
        let mut stream = tokio::net::TcpStream::connect(addr).await?;
        stream.write_all(request.as_bytes()).await?;
        let (mut raw, mut last, mut chunk) = (Vec::new(), None, [0u8; 8192]);
        loop {
            let read = stream.read(&mut chunk).await?;
            if read == 0 {
                return Ok::<_, std::io::Error>((raw, last));
            }
            raw.extend_from_slice(&chunk[..read]);
            last = Some(micros_since(origin));
        }
    })
    .await;
    attempt.outcome = match read {
        Err(_) => Outcome::Timeout,
        Ok(Err(error)) => Outcome::Failed(format!("{:?}", error.kind())),
        Ok(Ok((raw, last))) => {
            attempt.last_byte = last;
            answer(&raw).map_or_else(
                || Outcome::Failed("malformed".to_string()),
                Outcome::Answered,
            )
        }
    };
    attempt
}

fn answer(raw: &[u8]) -> Option<Answer> {
    let raw = String::from_utf8_lossy(raw);
    let status = raw.get(9..12)?.parse().ok()?;
    let body = raw.split_once("\r\n\r\n").map_or("", |(_, body)| body);
    let json: Option<serde_json::Value> = serde_json::from_str(body).ok();
    let source = json.as_ref().map(|json| &json["release_source"]);
    let mut reasons = json.as_ref().map(reasons).unwrap_or_default();
    reasons.sort();
    Some(Answer {
        status,
        parsed: json.is_some(),
        observation_status: source
            .and_then(|source| source["observation_status"].as_str())
            .unwrap_or_default()
            .to_string(),
        node_hostname: source.is_some_and(|source| source["node_hostname"].is_string()),
        reasons,
    })
}

const NONE: &str = "-";

fn opt(value: Option<impl ToString>) -> String {
    value.map_or_else(|| NONE.to_string(), |value| value.to_string())
}

fn write_run(
    out: &Path,
    conditions: &BTreeMap<String, String>,
    attempts: &[Attempt],
    ticks: &[(u64, u64)],
    resources: &Resources,
) {
    let lines = |rows: Vec<String>| rows.join("\n") + "\n";
    let conditions = conditions
        .iter()
        .map(|(key, value)| format!("{key}\t{value}"));
    std::fs::write(out.join("conditions.tsv"), lines(conditions.collect())).unwrap();
    let mut rows = vec![
        "endpoint\tseq\tmeasured\tscheduled_us\tsent_us\tlast_byte_us\toutcome\tstatus\tparsed\tobservation\thostname\treasons"
            .to_string(),
    ];
    for attempt in attempts {
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
    let ticks = ticks.iter().map(|(slot, woke)| format!("{slot}\t{woke}"));
    std::fs::write(out.join("ticks.tsv"), lines(ticks.collect())).unwrap();
    let mut rows = vec![
        format!("pool_max\t{}", resources.pool_max),
        format!("pg_statements\t{}", resources.pg_statements),
    ];
    for (point, sample) in [("start", resources.start), ("end", resources.end)] {
        rows.push(format!("{point}.threads\t{}", opt(sample.threads)));
        rows.push(format!("{point}.pool_size\t{}", sample.pool_size));
        rows.push(format!("{point}.pool_idle\t{}", sample.pool_idle));
        rows.push(format!("{point}.proxy_accepted\t{}", sample.proxy_accepted));
        rows.push(format!("{point}.proxy_open\t{}", sample.proxy_open));
    }
    std::fs::write(out.join("resources.tsv"), lines(rows)).unwrap();
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
/// and that no foreign build overlapped it.
fn load_run(dir: &Path, runner: bool) -> RunData {
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
        pool_size: number(&format!("{point}.pool_size")).unwrap_or(usize::MAX) as u32,
        pool_idle: number(&format!("{point}.pool_idle")).unwrap_or_default(),
        proxy_accepted: number(&format!("{point}.proxy_accepted")).unwrap_or_default(),
        proxy_open: number(&format!("{point}.proxy_open")).unwrap_or_default(),
    };
    run.resources = Resources {
        start: point("start"),
        end: point("end"),
        pool_max: number("pool_max").unwrap_or_default() as u32,
        pg_statements: number("pg_statements").unwrap_or_default(),
    };
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

fn compare_runs(spec: &str, out: &Path) {
    let (base, candidate) = spec
        .split_once(';')
        .unwrap_or_else(|| panic!("{COMPARE_ENV} is `base,...;candidate,...`"));
    let load = |dirs: &str| -> Vec<RunSummary> {
        dirs.split(',')
            .filter(|dir| !dir.is_empty())
            .map(|dir| verdict::summarize(&load_run(Path::new(dir), true)))
            .collect()
    };
    let (base, candidate) = (load(base), load(candidate));
    let comparison = verdict::compare(&base, &candidate);
    let mut rendered = render_comparison(&comparison);
    for (version, runs) in [("BASE", &base), ("CANDIDATE", &candidate)] {
        let (endpoints, ticks) = verdict::pooled(runs);
        rendered += &format!("\n== {version} pooled over {} runs\n", runs.len());
        rendered += &render_endpoints(&endpoints, &ticks);
        for run in runs.iter() {
            rendered += &format!("\n{}", render_run(run));
        }
    }
    std::fs::write(out.join("comparison.txt"), &rendered).expect("write comparison");
    println!("{rendered}");
}

fn render_endpoints(
    endpoints: &BTreeMap<Endpoint, EndpointSummary>,
    ticks: &[Option<u64>],
) -> String {
    let mut text = String::new();
    for (endpoint, summary) in endpoints {
        let c = summary.counts;
        let judged = |permille| verdict::percentile(&summary.judged, permille);
        text += &format!(
            "{}: attempts {} completed {} errors {} timeouts {} unsent {}\n  scheduled-basis p50 {} p95 {} p99 {} max {}\n  sent-basis p99 {} | send lag p99 {} max {}\n",
            endpoint.name(),
            c.attempts,
            c.completed,
            c.errors,
            c.timeouts,
            c.unsent,
            judged(500),
            judged(950),
            judged(990),
            judged(1000),
            verdict::percentile(&summary.sent_basis, 990),
            verdict::percentile(&summary.lag, 990),
            verdict::percentile(&summary.lag, 1000),
        );
    }
    let misses = ticks.iter().filter(|tick| tick.is_none()).count();
    text += &format!(
        "tick: slots {} misses {misses} | p50 {} p99 {} max {}\n",
        ticks.len(),
        verdict::percentile(ticks, 500),
        verdict::percentile(ticks, 990),
        verdict::percentile(ticks, 1000),
    );
    text
}

fn render_run(run: &RunSummary) -> String {
    let mut text = format!(
        "run {}\ninvalid: {}\nconditions:",
        run.name,
        listed(&run.invalid)
    );
    for (key, value) in &run.conditions {
        text += &format!(" {key}={value}");
    }
    text += "\n";
    for (endpoint, (status, reasons)) in &run.reference {
        text += &format!(
            "reference {}: status {status} reasons [{}]\n",
            endpoint.name(),
            reasons.join(",")
        );
    }
    text += &render_endpoints(&run.endpoints, &run.ticks);
    let r = &run.resources;
    for (point, sample) in [("start", r.start), ("end", r.end)] {
        text += &format!(
            "resources {point}: threads {} pool {}/{} idle (max {}) proxy accepted {} open {}\n",
            opt(sample.threads),
            sample.pool_size,
            sample.pool_idle,
            r.pool_max,
            sample.proxy_accepted,
            sample.proxy_open,
        );
    }
    text + &format!("pg statements in window: {}\n", r.pg_statements)
}

fn render_comparison(comparison: &Comparison) -> String {
    let mut text = format!(
        "VERDICT detail={} public={} tick={}\ninvalid: {}\n",
        comparison.detail.verdict,
        comparison.public.verdict,
        comparison.tick.verdict,
        listed(&comparison.invalid),
    );
    for (name, metric) in [
        ("detail p99", &comparison.detail),
        ("public p99", &comparison.public),
        ("tick p99", &comparison.tick),
    ] {
        text += &render_metric(name, metric);
    }
    text
}

fn render_metric(name: &str, metric: &MetricComparison) -> String {
    let runs = |stats: &[Stat]| {
        stats
            .iter()
            .map(Stat::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!(
        "{name}: {}\n  BASE runs [{}] pooled {} incomplete {}\n  CANDIDATE runs [{}] pooled {} incomplete {}\n  pooled difference {}\n",
        metric.verdict,
        runs(&metric.base_runs),
        metric.base_pooled,
        metric.base_incomplete,
        runs(&metric.candidate_runs),
        metric.candidate_pooled,
        metric.candidate_incomplete,
        metric
            .difference()
            .map_or("UNRESOLVED/INVALID".to_string(), |us| format!(
                "{:+.1}ms",
                us as f64 / 1000.0
            )),
    )
}

fn listed(reasons: &[String]) -> String {
    if reasons.is_empty() {
        "none".to_string()
    } else {
        reasons.join(" ")
    }
}
