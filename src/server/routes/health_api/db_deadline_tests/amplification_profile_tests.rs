//! `health_amplification_profile`: public and detail health under open-loop load on an isolated
//! PostgreSQL with a 1 s tick probe; profile `s2` adds policy read hooks; exact name only.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::amplification_policy_tests as policy;
use super::amplification_records_tests::{load_run, opt, write_run};
use super::amplification_verdict_tests::{
    self as verdict, Answer, Attempt, Comparison, Endpoint, EndpointSummary, Lane,
    MetricComparison, Outcome, Peak, Plan, Profile, Resources, RunData, RunSummary, SEC, Sample,
    Stat,
};
use super::resource_regression_tests::process_threads;
use super::{Db, reasons, registry_with_mailboxes, serve};

const OUT_ENV: &str = "ADK_HEALTH_AMP_OUT";
/// `s1` (default) or `s2`.
const PROFILE_ENV: &str = "ADK_HEALTH_AMP_PROFILE";
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
const HARNESS_SOURCES: [&str; 7] = [
    include_str!("amplification_profile_tests.rs"),
    include_str!("amplification_policy_tests.rs"),
    include_str!("amplification_records_tests.rs"),
    include_str!("amplification_verdict_tests.rs"),
    include_str!("amplification_verdict_tests/policy_verdict_tests.rs"),
    include_str!("resource_regression_tests.rs"),
    include_str!("../db_deadline_tests.rs"),
];
/// Spacing of the connection-high samples taken through the measured window.
const PEAK_EVERY: Duration = Duration::from_millis(100);

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
    let profile = std::env::var(PROFILE_ENV).map_or(Profile::S1, |name| {
        Profile::parse(&name).unwrap_or_else(|| panic!("{PROFILE_ENV}={name}"))
    });
    let plan = Plan {
        seed: SEED,
        rps: RPS,
        warmup_secs: env_secs(WARMUP_ENV, WARMUP_SECS),
        measure_secs: env_secs(MEASURE_ENV, MEASURE_SECS),
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    let root = crate::config::runtime_root().expect("the guard sets a runtime root");
    let runtime_dir = root.join("runtime");
    std::fs::create_dir_all(&runtime_dir).expect("create runtime dir");
    std::fs::write(runtime_dir.join("release-source.json"), MANIFEST).expect("write manifest");

    let infra = runtime("amp-infra", 2);
    let db = infra.block_on(async {
        let db = Db::start(Duration::ZERO, POOL_MAX).await;
        db.seed_blocks().await;
        db
    });
    let pg_version = infra.block_on(async {
        let connection = db.seed.acquire().await.expect("a seed connection");
        connection
            .server_version_num()
            .expect("the server reports its version")
    });
    let server = runtime("amp-server", SERVER_WORKERS);
    let (addr, engines) = server.block_on(async {
        let (engine, engines) = match profile {
            Profile::S1 => {
                let mut engine_config = crate::config::Config::default();
                engine_config.policies.hot_reload = false;
                let engine =
                    crate::engine::PolicyEngine::new_with_pg(&engine_config, Some(db.pool.clone()))
                        .expect("policy engine with PostgreSQL");
                (engine, None)
            }
            Profile::S2 => {
                let (main, tick) = policy::engines(&root, &db.pool);
                (main.clone(), Some((main, tick)))
            }
        };
        let registry = registry_with_mailboxes(&MAILBOXES).await;
        let tx = crate::server::ws::new_broadcast();
        let buf = crate::server::ws::spawn_batch_flusher(tx.clone());
        let addr = serve(crate::server::routes::api_router_with_pg(
            engine,
            crate::config::Config::default(),
            tx,
            buf,
            Some(registry),
            Some(db.pool.clone()),
        ))
        .await;
        (addr, engines)
    });
    if let Some((main, _)) = &engines {
        policy::seed(main);
    }

    let origin = Instant::now() + Duration::from_secs(1);
    let wakes = Arc::new(Mutex::new(Vec::new()));
    server.spawn(tick_probe(origin, plan.tick_slots(), Arc::clone(&wakes)));
    let generator = match profile {
        Profile::S1 => runtime("amp-load", GENERATOR_WORKERS),
        // Hook threads stay alive between hooks, so their count holds still through the window.
        Profile::S2 => tokio::runtime::Builder::new_multi_thread()
            .worker_threads(GENERATOR_WORKERS)
            .thread_name("amp-load")
            .thread_keep_alive(Duration::from_secs(3_600))
            .enable_all()
            .build()
            .expect("runtime"),
    };
    let load = generator.spawn(drive_load(addr, origin, plan));
    let policy_load = engines.as_ref().map(|(main, tick)| {
        (
            generator.spawn(policy::drive_hooks(main.clone(), Lane::Main, origin, plan)),
            generator.spawn(policy::drive_hooks(tick.clone(), Lane::Tick, origin, plan)),
            generator.spawn(policy::acquire_probe(db.pool.clone(), origin, plan)),
        )
    });
    sleep_until(origin + Duration::from_micros(plan.warmup_end()));
    let start = sample(&db);
    db.proxy.reset_executions();
    let window_end = origin + Duration::from_micros(plan.end());
    let peak = match profile {
        Profile::S1 => {
            sleep_until(window_end);
            Peak::default()
        }
        Profile::S2 => sample_peaks(&db, window_end),
    };
    let executions = db.proxy.executions();
    let end = sample(&db);
    let attempts = generator
        .block_on(load)
        .expect("the load generator finishes");
    let mut hooks = Vec::new();
    let mut acquires = Vec::new();
    if let (Some((main_hooks, tick_hooks, probe)), Some((main, tick))) = (policy_load, &engines) {
        for (fired, engine) in [(main_hooks, main), (tick_hooks, tick)] {
            let mut lane = generator.block_on(fired).expect("the hook driver finishes");
            policy::settle_reads(engine, &mut lane);
            hooks.extend(lane);
        }
        acquires = generator
            .block_on(probe)
            .expect("the acquire probe finishes");
    }
    drop(engines);
    let ticks = wakes.lock().unwrap().clone();
    server.block_on(db.pool.close());
    server.shutdown_timeout(Duration::from_secs(5));
    generator.shutdown_timeout(Duration::from_secs(5));
    infra.block_on(db.finish());

    let run = RunData {
        name: String::new(),
        conditions: conditions(plan, pg_version, profile),
        attempts,
        ticks,
        resources: Resources {
            start,
            end,
            pool_max: POOL_MAX,
            pg_statements: executions.len(),
            peak,
            policy_statements: (profile == Profile::S2)
                .then(|| policy::policy_statements(&executions)),
        },
        policy: hooks,
        acquires,
        invalid: Vec::new(),
    };
    write_run(out, &run);
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
        pool_size: Some(db.pool.size()),
        pool_idle: Some(db.pool.num_idle()),
        proxy_accepted: Some(db.proxy.accepted()),
        proxy_open: Some(db.proxy.open()),
    }
}

/// Highs of open proxy connections, server pool size and their difference until `until`.
fn sample_peaks(db: &Db, until: Instant) -> Peak {
    let (mut open, mut pool, mut beyond) = (0, 0, 0);
    loop {
        let (now_open, now_pool) = (db.proxy.open(), db.pool.size());
        open = open.max(now_open);
        pool = pool.max(now_pool);
        beyond = beyond.max(now_open.saturating_sub(now_pool as usize));
        if Instant::now() >= until {
            break;
        }
        sleep_until((Instant::now() + PEAK_EVERY).min(until));
    }
    Peak {
        proxy_open: Some(open),
        pool_size: Some(pool),
        beyond_pool: Some(beyond),
    }
}

fn conditions(plan: Plan, pg_version: u32, profile: Profile) -> BTreeMap<String, String> {
    let mut harness = Sha256::new();
    for source in HARNESS_SOURCES {
        harness.update(source.as_bytes());
    }
    let binary = std::fs::read(std::env::current_exe().expect("test binary path"))
        .expect("read the test binary");
    let mut conditions: BTreeMap<String, String> = [
        ("profile", profile.name().to_string()),
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
    .collect();
    if profile == Profile::S2 {
        for (key, value) in policy::conditions() {
            conditions.insert(key.to_string(), value);
        }
    }
    conditions
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
        let (lanes, acquires) = verdict::pooled_policy(runs);
        rendered += &render_policy(&lanes, &acquires);
        for run in runs.iter() {
            rendered += &format!("\n{}", render_run(run));
        }
    }
    std::fs::write(out.join("comparison.txt"), &rendered).expect("write comparison");
    println!("{rendered}");
}

/// One latency series: counts, scheduled-basis percentiles, sent-basis p99 and send lag.
fn render_series(name: &str, summary: &EndpointSummary) -> String {
    let c = summary.counts;
    let judged = |permille| verdict::percentile(&summary.judged, permille);
    format!(
        "{name}: attempts {} completed {} errors {} timeouts {} unsent {}\n  scheduled-basis p50 {} p95 {} p99 {} max {}\n  sent-basis p99 {} | send lag p99 {} max {}\n",
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
    )
}

/// Policy hook lanes and the server pool acquire probe; empty for a health-only run.
fn render_policy(lanes: &BTreeMap<Lane, EndpointSummary>, acquires: &[Option<u64>]) -> String {
    let mut text = String::new();
    for (lane, summary) in lanes {
        text += &render_series(&format!("policy {}", lane.name()), summary);
    }
    if !lanes.is_empty() {
        let failed = acquires.iter().filter(|wait| wait.is_none()).count();
        text += &format!(
            "server pool acquire: samples {} failed {failed} | wait p50 {} p99 {} max {}\n",
            acquires.len(),
            verdict::percentile(acquires, 500),
            verdict::percentile(acquires, 990),
            verdict::percentile(acquires, 1000),
        );
    }
    text
}

fn render_endpoints(
    endpoints: &BTreeMap<Endpoint, EndpointSummary>,
    ticks: &[Option<u64>],
) -> String {
    let mut text = String::new();
    for (endpoint, summary) in endpoints {
        text += &render_series(endpoint.name(), summary);
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
    text += &render_policy(&run.policy, &run.acquires);
    let r = &run.resources;
    for (point, sample) in [("start", r.start), ("end", r.end)] {
        text += &format!(
            "resources {point}: threads {} pool {}/{} idle (max {}) proxy accepted {} open {}\n",
            opt(sample.threads),
            opt(sample.pool_size),
            opt(sample.pool_idle),
            r.pool_max,
            opt(sample.proxy_accepted),
            opt(sample.proxy_open),
        );
    }
    if run.profile == Profile::S2 {
        text += &format!(
            "window highs: proxy open {} pool {} reader connections (open minus pool) {} | policy statements {}\n",
            opt(r.peak.proxy_open),
            opt(r.peak.pool_size),
            opt(r.peak.beyond_pool),
            opt(r.policy_statements),
        );
    }
    text + &format!("pg statements in window: {}\n", r.pg_statements)
}

fn render_comparison(comparison: &Comparison) -> String {
    let policy = comparison.policy.as_ref().map_or(String::new(), |policy| {
        format!(" policy={}", policy.verdict)
    });
    let mut text = format!(
        "VERDICT detail={} public={} tick={}{policy}\ninvalid: {}\n",
        comparison.detail.verdict,
        comparison.public.verdict,
        comparison.tick.verdict,
        listed(&comparison.invalid),
    );
    for (name, metric) in [
        ("detail p99", Some(&comparison.detail)),
        ("public p99", Some(&comparison.public)),
        ("tick p99", Some(&comparison.tick)),
        ("policy main p99", comparison.policy.as_ref()),
    ] {
        if let Some(metric) = metric {
            text += &render_metric(name, metric);
        }
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
