//! The `s2` profile's policy load: a test policy whose hook makes ten fixed `config.get` and
//! `kv.get` reads, fired open-loop on PostgreSQL-backed main and tick engines.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use super::amplification_verdict_tests::{
    Acquire, Lane, Plan, PolicyAttempt, PolicyOutcome, RELOADED, SEC,
};

pub(super) const HOOK: &str = "onAmplificationRead";
/// Main-engine hook spacing unless overridden; the tick engine fires once a second.
const MAIN_INTERVAL_MS: u64 = 250;
const MAIN_INTERVAL_ENV: &str = "ADK_HEALTH_AMP_POLICY_INTERVAL_MS";
const TICK_INTERVAL_MS: u64 = 1_000;
/// Three scalars and five kv keys read once, two blob fallbacks that miss the scalar first.
pub(super) const SELECTS_PER_HOOK: usize = 12;
/// Main, tick and each one's hot-reload Context register the PostgreSQL ops.
pub(super) const CONTEXTS_WITH_PG: usize = 4;
pub(super) const READING_CONTEXTS: usize = 2;
const HOOK_DRAIN: Duration = Duration::from_secs(10);
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);

/// `(kind, key, expected JSON)`; the two `amp_gone` keys are seeded already expired.
const READS: [(&str, &str, &str); 10] = [
    ("config", "amp_cfg_1", r#""a1""#),
    ("config", "amp_cfg_2", r#""a2""#),
    ("config", "amp_cfg_3", r#""a3""#),
    ("config", "dispatchRateLimitGateDangerPct", "77"),
    ("config", "maxEntryRetries", "7"),
    ("kv", "amp_kv_1", r#""k1""#),
    ("kv", "amp_kv_2", r#""k2""#),
    ("kv", "amp_kv_3", r#""k3""#),
    ("kv", "amp_gone_1", "null"),
    ("kv", "amp_gone_2", "null"),
];

pub(super) fn main_interval_ms() -> u64 {
    std::env::var(MAIN_INTERVAL_ENV).map_or(MAIN_INTERVAL_MS, |value| {
        value
            .parse()
            .unwrap_or_else(|_| panic!("{MAIN_INTERVAL_ENV}={value}"))
    })
}

pub(super) fn interval_ms(lane: Lane) -> u64 {
    match lane {
        Lane::Main => main_interval_ms(),
        Lane::Tick => TICK_INTERVAL_MS,
    }
}

pub(super) fn conditions() -> Vec<(&'static str, String)> {
    vec![
        (
            Lane::Main.interval_key(),
            interval_ms(Lane::Main).to_string(),
        ),
        (
            Lane::Tick.interval_key(),
            interval_ms(Lane::Tick).to_string(),
        ),
        ("reads_per_hook", READS.len().to_string()),
        ("selects_per_hook", SELECTS_PER_HOOK.to_string()),
        ("contexts_with_pg", CONTEXTS_WITH_PG.to_string()),
        ("reading_contexts", READING_CONTEXTS.to_string()),
        ("hot_reload", "true".to_string()),
    ]
}

/// The hook records `read` or what differed under `lane:seq`; a kv error is never a miss.
fn policy_source() -> String {
    let reads = serde_json::to_string(&READS).unwrap();
    format!(
        r#"var READS = {reads};
agentdesk.registerPolicy({{
    name: "amplification-read",
    {HOOK}: function(payload) {{
        var bad = [];
        for (var i = 0; i < READS.length; i++) {{
            var kind = READS[i][0], key = READS[i][1], want = READS[i][2], got;
            if (kind === "config") {{
                got = agentdesk.config.get(key);
            }} else {{
                var raw = JSON.parse(agentdesk.kv.__getRaw(key));
                if (raw.error) {{
                    bad.push(key + " error " + raw.error);
                    continue;
                }}
                got = raw.found ? raw.value : null;
            }}
            if (JSON.stringify(got) !== want) bad.push(key + "=" + JSON.stringify(got));
        }}
        globalThis.__ampReads = globalThis.__ampReads || {{}};
        globalThis.__ampReads[payload.lane + ":" + payload.seq] = bad.length ? bad.join("; ") : "read";
    }}
}});
"#
    )
}

fn policy_dir(root: &Path) -> std::path::PathBuf {
    root.join("amplification-policies")
}

/// Written before the database starts, so the file's own creation is old news to the watchers.
pub(super) fn write_policy(root: &Path) {
    let dir = policy_dir(root);
    std::fs::create_dir_all(&dir).expect("create the policy dir");
    std::fs::write(dir.join("amplification-read.js"), policy_source()).expect("write the policy");
}

/// Main and tick engines on `pool`, hot reload on as in operation, loading only the test policy.
pub(super) fn engines(
    root: &Path,
    pool: &sqlx::PgPool,
) -> (crate::engine::PolicyEngine, crate::engine::PolicyEngine) {
    let dir = policy_dir(root);
    let mut config = crate::config::Config::default();
    config.policies.dir = dir;
    config.policies.hot_reload = true;
    let main = crate::engine::PolicyEngine::new_with_pg(&config, Some(pool.clone()))
        .expect("main policy engine with PostgreSQL");
    let tick = crate::engine::PolicyEngine::new_for_tick(&config, Some(pool.clone()))
        .expect("tick policy engine with PostgreSQL");
    (main, tick)
}

/// Seeds the read keys through the existing kv write bridge and waits for the expired pair.
pub(super) fn seed(engine: &crate::engine::PolicyEngine) {
    let writes = r#"JSON.stringify([
        JSON.parse(agentdesk.kv.delete("dispatchRateLimitGateDangerPct")),
        JSON.parse(agentdesk.kv.delete("maxEntryRetries")),
        agentdesk.kv.set("runtime-config", '{"dispatchRateLimitGateDangerPct":77,"maxEntryRetries":7}', 0),
        agentdesk.kv.set("amp_cfg_1", "a1", 0),
        agentdesk.kv.set("amp_cfg_2", "a2", 0),
        agentdesk.kv.set("amp_cfg_3", "a3", 0),
        agentdesk.kv.set("amp_kv_1", "k1", 0),
        agentdesk.kv.set("amp_kv_2", "k2", 0),
        agentdesk.kv.set("amp_kv_3", "k3", 0),
        agentdesk.kv.set("amp_gone_1", "x", 1),
        agentdesk.kv.set("amp_gone_2", "x", 1)
    ])"#;
    let answers: Vec<serde_json::Value> = serde_json::from_str(
        &engine
            .eval_js::<String>(writes)
            .expect("seed the read keys"),
    )
    .expect("seed answers");
    assert!(
        answers.iter().all(|answer| answer["ok"] == true),
        "{answers:?}"
    );
    let expired =
        r#"agentdesk.kv.get("amp_gone_1") === null && agentdesk.kv.get("amp_gone_2") === null"#;
    let deadline = Instant::now() + Duration::from_secs(10);
    while !engine
        .eval_js::<bool>(expired)
        .expect("read the expired keys")
    {
        assert!(Instant::now() < deadline, "the seeded keys never expired");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn micros_since(origin: Instant) -> u64 {
    Instant::now().saturating_duration_since(origin).as_micros() as u64
}

/// Open loop: each hook fires at its scheduled time on a blocking thread, so a slow actor queues
/// hooks without delaying sends; unsent at the window's end or running after the drain is recorded.
pub(super) async fn drive_hooks(
    engine: crate::engine::PolicyEngine,
    lane: Lane,
    origin: Instant,
    plan: Plan,
) -> Vec<PolicyAttempt> {
    let mut attempts = Vec::new();
    let mut in_flight = Vec::new();
    for (seq, scheduled) in plan
        .policy_schedule(lane, interval_ms(lane))
        .into_iter()
        .enumerate()
    {
        tokio::time::sleep_until((origin + Duration::from_micros(scheduled)).into()).await;
        let attempt = PolicyAttempt {
            lane,
            seq,
            measured: scheduled >= plan.warmup_end(),
            scheduled,
            sent: None,
            end: None,
            outcome: PolicyOutcome::Unsent,
        };
        if micros_since(origin) >= plan.end() {
            attempts.push(attempt);
            continue;
        }
        let engine = engine.clone();
        let payload = serde_json::json!({ "lane": lane.name(), "seq": seq });
        let fired = tokio::task::spawn_blocking(move || {
            let sent = micros_since(origin);
            let result = engine.try_fire_hook_by_name(HOOK, payload);
            (
                sent,
                result.map_err(|error| error.to_string()),
                micros_since(origin),
            )
        });
        in_flight.push((attempt, fired));
    }
    let drain = origin + Duration::from_micros(plan.end()) + HOOK_DRAIN;
    for (mut attempt, fired) in in_flight {
        attempt.outcome = match tokio::time::timeout_at(drain.into(), fired).await {
            Ok(Ok((sent, result, end))) => {
                attempt.sent = Some(sent);
                attempt.end = Some(end);
                result.map_or_else(PolicyOutcome::Failed, |()| PolicyOutcome::Read)
            }
            Ok(Err(error)) => PolicyOutcome::Failed(format!("hook task: {error}")),
            Err(_) => PolicyOutcome::Timeout,
        };
        attempts.push(attempt);
    }
    attempts
}

/// A returned hook counts as read only when the policy recorded all ten values as expected; a
/// probe hook missing from this Context means a hot reload moved later hooks to another one.
pub(super) fn settle_reads(engine: &crate::engine::PolicyEngine, attempts: &mut [PolicyAttempt]) {
    let probe =
        engine.try_fire_hook_by_name(HOOK, serde_json::json!({ "lane": "probe", "seq": 0 }));
    let recorded: BTreeMap<String, String> = engine
        .eval_js::<String>("JSON.stringify(globalThis.__ampReads || {})")
        .ok()
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default();
    let missing = match probe.is_ok() && !recorded.contains_key("probe:0") {
        true => RELOADED,
        false => "unrecorded",
    };
    for attempt in attempts {
        if attempt.outcome != PolicyOutcome::Read {
            continue;
        }
        let key = format!("{}:{}", attempt.lane.name(), attempt.seq);
        match recorded.get(&key).map(String::as_str) {
            Some("read") => {}
            Some(detail) => attempt.outcome = PolicyOutcome::Mismatch(detail.to_string()),
            None => attempt.outcome = PolicyOutcome::Mismatch(missing.to_string()),
        }
    }
}

/// One server pool acquire a second, timed from the call to the connection in hand.
pub(super) async fn acquire_probe(pool: sqlx::PgPool, origin: Instant, plan: Plan) -> Vec<Acquire> {
    let mut acquires = Vec::new();
    for second in 1..plan.end() / SEC {
        tokio::time::sleep_until((origin + Duration::from_secs(second)).into()).await;
        let started = Instant::now();
        let waited = match tokio::time::timeout(ACQUIRE_TIMEOUT, pool.acquire()).await {
            Ok(Ok(connection)) => {
                let waited = started.elapsed().as_micros() as u64;
                drop(connection);
                Some(waited)
            }
            _ => None,
        };
        acquires.push(Acquire {
            measured: second * SEC >= plan.warmup_end(),
            waited,
        });
    }
    acquires
}

/// Window executions of the policy read SELECTs; health reads kv_meta by `ANY($1)` instead.
pub(super) fn policy_statements(executions: &[String]) -> usize {
    executions
        .iter()
        .filter(|sql| sql.contains("FROM kv_meta") && sql.contains("WHERE key = $1"))
        .count()
}
