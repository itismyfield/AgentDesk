//! Reclaims PostgreSQL test databases whose owning test process provably died.
//! Only marked fixtures on an opted-in server are dropped; see `docs/runbooks/orphan-test-database-cleanup.md`.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, OnceLock};
use std::time::{Duration, Instant};

use regex::Regex;
use sqlx::PgPool;

use crate::services::process::{ProcessIdentity, ProcessIdentityProbe};

/// Written only by `mark_test_database`; matched with `starts_with`, never `LIKE`.
const MARKER_PREFIX: &str = "agentdesk-test-fixture created_at_unix=";
/// Reserved for in-flight creates; the name carries the server epoch of the CREATE.
const PENDING_PREFIX: &str = "agentdesk_pending_";
const OPT_IN_ENV: &str = "AGENTDESK_TEST_PG_RECLAIM_SERVER";
const DENY_ENV: &str = "AGENTDESK_TEST_PG_RECLAIM_DENY_SERVERS";
const LOG_ENV: &str = "AGENTDESK_TEST_PG_RECLAIM_LOG";
/// Test seam: a child process stops at this create stage until stdin yields a line.
const PAUSE_ENV: &str = "AGENTDESK_TEST_PG_RECLAIM_PAUSE_AT";

/// Never dropped, whatever their comment says.
const PROTECTED_DATABASE_NAMES: [&str; 5] =
    ["postgres", "template0", "template1", "agentdesk", "memento"];
/// A server hosting any of these is operational and is never swept.
const OPERATIONAL_DATABASE_NAMES: [&str; 2] = ["agentdesk", "memento"];
/// The canonical control-plane server, refused even when opted in.
const CANONICAL_SERVER_SYSIDS: [u64; 1] = [7_625_542_565_928_183_939];
/// Per test process, not per server: N processes may drop up to N times this.
const RECLAIM_MAX_DROPS_PER_PROCESS: usize = 8;
/// No new DROP starts after this; one already running may take its own timeout.
const RECLAIM_DROP_BUDGET: Duration = Duration::from_secs(20);

static SWEPT_THIS_PROCESS: AtomicBool = AtomicBool::new(false);

static MARKER_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(
        r"^agentdesk-test-fixture created_at_unix=([0-9]{1,12})(?: host=([A-Za-z0-9._-]{1,64}) pid=([0-9]{1,10}) start=([0-9]{1,39}|-) lstart=([0-9]{1,39}|-))?$",
    )
    .ok()
});

#[derive(Clone, Debug, PartialEq, Eq)]
struct Owner {
    host: String,
    pid: u32,
    start: Option<u128>,
    lstart: Option<u128>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FixtureRow {
    name: String,
    oid: i64,
    owner_role: i64,
    comment: Option<String>,
    is_template: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verdict {
    OwnerDead,
    OwnerAlive,
    OwnerUnproven,
    Malformed,
}

#[derive(Clone, Debug)]
struct Classified {
    row: FixtureRow,
    created_at: Option<i64>,
    owner: Option<Owner>,
    verdict: Verdict,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Gate {
    Reclaim,
    ListOnly(&'static str),
}

struct GateInputs<'a> {
    sysid: Option<u64>,
    operational_present: bool,
    opt_in: Option<&'a str>,
    deny: Option<&'a str>,
}

/// Server clock, so hosts sharing one test server agree on every database's age.
async fn server_now_unix(admin_pool: &PgPool) -> Result<i64, String> {
    super::run_test_postgres_sqlx_op(
        "read postgres server clock",
        sqlx::query_scalar::<_, i64>("SELECT extract(epoch FROM clock_timestamp())::bigint")
            .fetch_one(admin_pool),
    )
    .await
}

fn pending_database_name(created_at_unix: i64) -> String {
    format!(
        "{PENDING_PREFIX}{created_at_unix}_{}",
        uuid::Uuid::new_v4().simple()
    )
}

fn valid_host(host: &str) -> bool {
    host != "localhost"
        && (1..=64).contains(&host.len())
        && host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
}

fn local_host() -> Option<&'static str> {
    static HOST: OnceLock<Option<String>> = OnceLock::new();
    HOST.get_or_init(|| {
        let host = crate::services::platform::shell::hostname_short();
        (valid_host(&host) && host != "-").then_some(host)
    })
    .as_deref()
}

/// Captured once: on macOS the identity read may spawn `ps`.
fn own_owner() -> &'static Owner {
    static OWN: OnceLock<Owner> = OnceLock::new();
    OWN.get_or_init(|| {
        let pid = std::process::id();
        let identity = ProcessIdentity::capture(pid);
        Owner {
            host: local_host().unwrap_or("-").to_string(),
            pid,
            start: identity.persisted_starttime(),
            lstart: identity.persisted_macos_lstart_hash(),
        }
    })
}

fn marker_text(created_at_unix: i64, owner: &Owner) -> String {
    let field = |value: Option<u128>| value.map_or_else(|| "-".to_string(), |v| v.to_string());
    format!(
        "{MARKER_PREFIX}{created_at_unix} host={} pid={} start={} lstart={}",
        owner.host,
        owner.pid,
        field(owner.start),
        field(owner.lstart)
    )
}

/// `None` means malformed. A v1 marker parses with no owner.
fn parse_marker(comment: &str) -> Option<(i64, Option<Owner>)> {
    let captures = MARKER_RE.as_ref()?.captures(comment)?;
    let created_at = captures.get(1)?.as_str().parse().ok()?;
    let Some(host) = captures.get(2) else {
        return Some((created_at, None));
    };
    let number = |index: usize| match captures.get(index)?.as_str() {
        "-" => Some(None),
        digits => digits.parse::<u128>().ok().map(Some),
    };
    // kill(2) treats 0 and negative pids as process groups.
    let pid = captures
        .get(3)?
        .as_str()
        .parse::<u32>()
        .ok()
        .filter(|pid| (1..=i32::MAX as u32).contains(pid))?;
    let owner = Owner {
        host: host.as_str().to_string(),
        pid,
        start: number(4)?,
        lstart: number(5)?,
    };
    Some((created_at, Some(owner)))
}

fn is_pending_name(name: &str) -> bool {
    name.strip_prefix(PENDING_PREFIX)
        .and_then(|rest| rest.split_once('_'))
        .is_some_and(|(epoch, uuid)| {
            (1..=12).contains(&epoch.len())
                && epoch.bytes().all(|byte| byte.is_ascii_digit())
                && uuid.len() == 32
                && uuid
                    .bytes()
                    .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        })
}

#[cfg(unix)]
fn probe_owner(owner: &Owner) -> ProcessIdentityProbe {
    ProcessIdentity::from_persisted(owner.start, owner.lstart).probe(owner.pid)
}

#[cfg(not(unix))]
fn probe_owner(_owner: &Owner) -> ProcessIdentityProbe {
    ProcessIdentityProbe::ProbeError
}

/// Only a same-host owner whose identity probe says gone or reused is dead; age proves nothing.
fn owner_verdict(
    owner: Option<&Owner>,
    local_host: Option<&str>,
    probe: &impl Fn(&Owner) -> ProcessIdentityProbe,
) -> Verdict {
    let Some(owner) = owner else {
        return Verdict::OwnerUnproven;
    };
    if local_host.is_none_or(|host| host != owner.host) {
        return Verdict::OwnerUnproven;
    }
    match probe(owner) {
        ProcessIdentityProbe::Same => Verdict::OwnerAlive,
        ProcessIdentityProbe::GoneOrReused => Verdict::OwnerDead,
        ProcessIdentityProbe::ProbeError => Verdict::OwnerUnproven,
    }
}

fn classify_row(
    row: &FixtureRow,
    local_host: Option<&str>,
    probe: &impl Fn(&Owner) -> ProcessIdentityProbe,
) -> Classified {
    let (created_at, owner, verdict) = match row.comment.as_deref().map(parse_marker) {
        Some(Some((created_at, owner))) => {
            let verdict = owner_verdict(owner.as_ref(), local_host, probe);
            (Some(created_at), owner, verdict)
        }
        Some(None) => (None, None, Verdict::Malformed),
        None if is_pending_name(&row.name) => (None, None, Verdict::OwnerUnproven),
        None => (None, None, Verdict::Malformed),
    };
    Classified {
        row: row.clone(),
        created_at,
        owner,
        verdict,
    }
}

/// A protected or template database among the rows aborts the whole sweep.
fn classify(
    rows: &[FixtureRow],
    local_host: Option<&str>,
    probe: &impl Fn(&Owner) -> ProcessIdentityProbe,
) -> Result<Vec<Classified>, String> {
    rows.iter()
        .map(|row| {
            if row.is_template || PROTECTED_DATABASE_NAMES.contains(&row.name.as_str()) {
                return Err(format!(
                    "protected database {} listed as a test fixture",
                    row.name
                ));
            }
            Ok(classify_row(row, local_host, probe))
        })
        .collect()
}

fn parse_sysid(value: &str) -> Option<u64> {
    let value = value.trim();
    if value.is_empty() || value.len() > 20 || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

/// Every refusal is independent of the opt-in; malformed protective input refuses too.
fn decide_gate(inputs: &GateInputs<'_>) -> Gate {
    let Some(sysid) = inputs.sysid else {
        return Gate::ListOnly("ServerIdentityUnreadable");
    };
    if CANONICAL_SERVER_SYSIDS.contains(&sysid) {
        return Gate::ListOnly("DeniedServer");
    }
    if let Some(deny) = inputs.deny {
        match deny.split(',').map(parse_sysid).collect::<Option<Vec<_>>>() {
            None => return Gate::ListOnly("DenyListMalformed"),
            Some(denied) if denied.contains(&sysid) => return Gate::ListOnly("DeniedServer"),
            Some(_) => {}
        }
    }
    if inputs.operational_present {
        return Gate::ListOnly("OperationalDatabasePresent");
    }
    match inputs.opt_in.map(parse_sysid) {
        None => Gate::ListOnly("NotOptedIn"),
        Some(None) => Gate::ListOnly("OptInMalformed"),
        Some(Some(opted)) if opted == sysid => Gate::Reclaim,
        Some(Some(_)) => Gate::ListOnly("OptInMismatch"),
    }
}

/// Oldest owner-dead databases first, capped per process; returns the plan and the deferred count.
fn plan_drops(classified: &[Classified]) -> (Vec<&Classified>, usize) {
    let mut dead: Vec<&Classified> = classified
        .iter()
        .filter(|entry| entry.verdict == Verdict::OwnerDead)
        .collect();
    dead.sort_by(|a, b| (a.created_at, &a.row.name).cmp(&(b.created_at, &b.row.name)));
    let deferred = dead.len().saturating_sub(RECLAIM_MAX_DROPS_PER_PROCESS);
    dead.truncate(RECLAIM_MAX_DROPS_PER_PROCESS);
    (dead, deferred)
}

trait SyncData {
    fn sync_data(&self) -> std::io::Result<()>;
}

impl SyncData for std::fs::File {
    fn sync_data(&self) -> std::io::Result<()> {
        std::fs::File::sync_data(self)
    }
}

struct AuditLog<W> {
    out: W,
}

impl<W: Write + SyncData> AuditLog<W> {
    /// A line counts as recorded only after write, flush and sync all succeed.
    fn record(&mut self, line: &serde_json::Value) -> std::io::Result<()> {
        writeln!(self.out, "{line}")?;
        self.out.flush()?;
        self.out.sync_data()
    }
}

fn open_audit_log(sysid: Option<u64>) -> std::io::Result<AuditLog<std::fs::File>> {
    let path = std::env::var_os(LOG_ENV)
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            let server = sysid.map_or_else(|| "unknown".to_string(), |id| id.to_string());
            std::env::temp_dir().join(format!("agentdesk-pg-reclaim-{server}.jsonl"))
        });
    let out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    Ok(AuditLog { out })
}

struct CurrentRow {
    oid: i64,
    owner_role: i64,
    comment: Option<String>,
    is_template: bool,
    has_session: bool,
}

trait ReclaimBackend {
    async fn current(&mut self, name: &str) -> Result<Option<CurrentRow>, String>;
    async fn drop_database(&mut self, name: &str) -> Result<(), String>;
}

struct PgBackend<'a> {
    pool: &'a PgPool,
    label: &'a str,
}

impl ReclaimBackend for PgBackend<'_> {
    async fn current(&mut self, name: &str) -> Result<Option<CurrentRow>, String> {
        let row = super::run_test_postgres_sqlx_op(
            &format!("{} recheck postgres test db {name}", self.label),
            sqlx::query_as::<_, (i64, i64, Option<String>, bool, bool)>(
                "SELECT d.oid::bigint, d.datdba::bigint, shobj_description(d.oid, 'pg_database'),
                        d.datistemplate,
                        EXISTS (SELECT 1 FROM pg_stat_activity a WHERE a.datname = d.datname)
                 FROM pg_database d WHERE d.datname = $1",
            )
            .bind(name)
            .fetch_optional(self.pool),
        )
        .await?;
        Ok(row.map(
            |(oid, owner_role, comment, is_template, has_session)| CurrentRow {
                oid,
                owner_role,
                comment,
                is_template,
                has_session,
            },
        ))
    }

    async fn drop_database(&mut self, name: &str) -> Result<(), String> {
        if !super::is_safe_test_database_name(name) {
            return Err(format!("unsafe postgres test database name {name}"));
        }
        // No FORCE: a database that gained a session fails the DROP instead of losing it.
        super::run_test_postgres_sqlx_op(
            &format!("{} reclaim postgres test db {name}", self.label),
            sqlx::query(&format!("DROP DATABASE IF EXISTS \"{name}\"")).execute(self.pool),
        )
        .await
        .map(|_| ())
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Outcome {
    dropped: Vec<String>,
    kept: Vec<String>,
    stopped: Option<String>,
}

/// Rechecks each planned database, records intent durably, then drops; any log or DROP error stops the run.
async fn execute<B: ReclaimBackend, W: Write + SyncData>(
    plan: &[&Classified],
    sysid: u64,
    log: &mut AuditLog<W>,
    backend: &mut B,
    local_host: Option<&str>,
    probe: &impl Fn(&Owner) -> ProcessIdentityProbe,
    budget: Duration,
) -> Outcome {
    let started = Instant::now();
    let mut outcome = Outcome::default();
    for planned in plan {
        let name = planned.row.name.as_str();
        if started.elapsed() >= budget {
            outcome.stopped = Some("drop time budget spent".to_string());
            break;
        }
        let current = match backend.current(name).await {
            Ok(current) => current,
            Err(error) => {
                outcome.stopped = Some(error);
                break;
            }
        };
        // Same object, still idle, and its owner still provably gone.
        let unchanged = current.is_some_and(|now| {
            now.oid == planned.row.oid
                && now.owner_role == planned.row.owner_role
                && now.comment == planned.row.comment
                && !now.is_template
                && !now.has_session
        }) && classify_row(&planned.row, local_host, probe).verdict
            == Verdict::OwnerDead;
        let entry = |phase: &str, result: &str| {
            serde_json::json!({
                "ts": chrono::Utc::now().to_rfc3339(),
                "phase": phase,
                "server_sysid": sysid.to_string(),
                "datname": name,
                "oid": planned.row.oid,
                "created_at_unix": planned.created_at,
                "owner_host": planned.owner.as_ref().map(|owner| owner.host.as_str()),
                "owner_pid": planned.owner.as_ref().map(|owner| owner.pid),
                "result": result,
            })
        };
        if !unchanged {
            outcome.kept.push(name.to_string());
            if let Err(error) = log.record(&entry("result", "kept: changed since scan")) {
                outcome.stopped = Some(format!("audit log write failed: {error}"));
                break;
            }
            continue;
        }
        if let Err(error) = log.record(&entry("intent", "drop")) {
            outcome.stopped = Some(format!("audit log write failed: {error}"));
            break;
        }
        let dropped = backend.drop_database(name).await;
        let result = match &dropped {
            Ok(()) => "dropped".to_string(),
            Err(error) => format!("error: {error}"),
        };
        if dropped.is_ok() {
            outcome.dropped.push(name.to_string());
        }
        if let Err(error) = log.record(&entry("result", &result)) {
            outcome.stopped = Some(format!("audit log write failed: {error}"));
            break;
        }
        if let Err(error) = dropped {
            outcome.stopped = Some(error);
            break;
        }
    }
    outcome
}

/// Writes this process's v2 marker; the creation time is the server epoch.
pub(super) async fn mark_test_database(
    admin_pool: &PgPool,
    database_name: &str,
    created_at_unix: i64,
    label: &str,
) -> Result<(), String> {
    let marker = marker_text(created_at_unix, own_owner());
    super::run_test_postgres_sqlx_op(
        &format!("{label} mark postgres test db {database_name}"),
        sqlx::query(&format!(
            "COMMENT ON DATABASE \"{database_name}\" IS '{marker}'"
        ))
        .execute(admin_pool),
    )
    .await
    .map(|_| ())
}

fn pause_at(stage: &str, database_name: &str) {
    if std::env::var(PAUSE_ENV).ok().as_deref() != Some(stage) {
        return;
    }
    println!("RECLAIM_CHILD PAUSED {stage} {database_name}");
    let _ = std::io::stdin().read_line(&mut String::new());
}

/// CREATE first uses a reserved pending name. Before COMMENT, that name
/// identifies the fixture; after COMMENT, the marker survives the RENAME.
pub(super) async fn create_marked(
    admin_pool: &PgPool,
    database_name: &str,
    label: &str,
) -> Result<(), String> {
    let created_at = server_now_unix(admin_pool).await?;
    let pending = pending_database_name(created_at);
    super::run_test_postgres_sqlx_op(
        &format!("{label} create postgres test db {database_name}"),
        sqlx::query(&format!("CREATE DATABASE \"{pending}\"")).execute(admin_pool),
    )
    .await?;
    pause_at("after_create", &pending);
    let finished = async {
        mark_test_database(admin_pool, &pending, created_at, label).await?;
        pause_at("after_comment", &pending);
        super::run_test_postgres_sqlx_op(
            &format!("{label} rename postgres test db {database_name}"),
            sqlx::query(&format!(
                "ALTER DATABASE \"{pending}\" RENAME TO \"{database_name}\""
            ))
            .execute(admin_pool),
        )
        .await
        .map(|_| ())
    }
    .await;
    if finished.is_err() {
        // Only the pending name is ours; a RENAME collision must not touch `database_name`.
        let dropped = super::run_test_postgres_sqlx_op(
            &format!("{label} drop pending postgres test db {pending}"),
            sqlx::query(&format!("DROP DATABASE IF EXISTS \"{pending}\"")).execute(admin_pool),
        )
        .await;
        if let Err(error) = dropped {
            tracing::warn!(label, error, "left pending postgres test db for the sweep");
        }
    } else {
        pause_at("after_rename", database_name);
    }
    finished
}

/// Server identity plus whether the server hosts an operational database.
async fn observe_server(admin_pool: &PgPool) -> Result<(Option<u64>, bool), String> {
    let sysid = super::run_test_postgres_sqlx_op(
        "read postgres system identifier",
        sqlx::query_scalar::<_, String>("SELECT system_identifier::text FROM pg_control_system()")
            .fetch_one(admin_pool),
    )
    .await
    .ok()
    .and_then(|value| parse_sysid(&value));
    let operational_present = super::run_test_postgres_sqlx_op(
        "check operational postgres databases",
        sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname = ANY($1))",
        )
        .bind(OPERATIONAL_DATABASE_NAMES.map(str::to_string).to_vec())
        .fetch_one(admin_pool),
    )
    .await?;
    Ok((sysid, operational_present))
}

/// Marked or still-pending databases with no connected session.
async fn list_fixture_rows(admin_pool: &PgPool) -> Result<Vec<FixtureRow>, String> {
    let rows = super::run_test_postgres_sqlx_op(
        "list postgres test fixture dbs",
        sqlx::query_as::<_, (String, i64, i64, Option<String>, bool)>(
            "SELECT d.datname::text, d.oid::bigint, d.datdba::bigint,
                    shobj_description(d.oid, 'pg_database'), d.datistemplate
             FROM pg_database d
             WHERE (starts_with(coalesce(shobj_description(d.oid, 'pg_database'), ''), $1)
                    OR (shobj_description(d.oid, 'pg_database') IS NULL
                        AND starts_with(d.datname, $2)))
               AND NOT EXISTS (SELECT 1 FROM pg_stat_activity a WHERE a.datname = d.datname)
             ORDER BY d.datname",
        )
        .bind(MARKER_PREFIX)
        .bind(PENDING_PREFIX)
        .fetch_all(admin_pool),
    )
    .await?;
    Ok(rows
        .into_iter()
        .map(|(name, oid, owner_role, comment, is_template)| FixtureRow {
            name,
            oid,
            owner_role,
            comment,
            is_template,
        })
        .collect())
}

fn env_value(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

fn summary(sysid: Option<u64>, classified: &[Classified]) -> String {
    let count = |verdict| {
        classified
            .iter()
            .filter(|entry| entry.verdict == verdict)
            .count()
    };
    format!(
        "pg test db reclaim (per-process, at most {RECLAIM_MAX_DROPS_PER_PROCESS} drops) on server {}: owner-dead {}, unproven {}, alive {}, malformed {}",
        sysid.map_or_else(|| "unknown".to_string(), |id| id.to_string()),
        count(Verdict::OwnerDead),
        count(Verdict::OwnerUnproven),
        count(Verdict::OwnerAlive),
        count(Verdict::Malformed),
    )
}

async fn sweep(admin_pool: &PgPool, label: &str) -> Result<(), String> {
    let (sysid, operational_present) = observe_server(admin_pool).await?;
    let rows = list_fixture_rows(admin_pool).await?;
    if rows.is_empty() {
        return Ok(());
    }
    let classified = classify(&rows, local_host(), &probe_owner)?;
    let (opt_in, deny) = (env_value(OPT_IN_ENV), env_value(DENY_ENV));
    let mut gate = decide_gate(&GateInputs {
        sysid,
        operational_present,
        opt_in: opt_in.as_deref(),
        deny: deny.as_deref(),
    });
    let (plan, deferred) = plan_drops(&classified);
    let mut log = None;
    if gate == Gate::Reclaim && !plan.is_empty() {
        match open_audit_log(sysid) {
            Ok(opened) => log = Some(opened),
            Err(_) => gate = Gate::ListOnly("LogUnavailable"),
        }
    }
    let summary = summary(sysid, &classified);
    let see = "see docs/runbooks/orphan-test-database-cleanup.md";
    match (gate, log, sysid) {
        (Gate::Reclaim, Some(mut log), Some(sysid)) => {
            let mut backend = PgBackend {
                pool: admin_pool,
                label,
            };
            let outcome = execute(
                &plan,
                sysid,
                &mut log,
                &mut backend,
                local_host(),
                &probe_owner,
                RECLAIM_DROP_BUDGET,
            )
            .await;
            eprintln!(
                "{summary}; gate=Reclaim; dropped {:?}, kept {:?}, deferred {deferred}, stopped {:?}; {see}",
                outcome.dropped, outcome.kept, outcome.stopped
            );
        }
        (Gate::ListOnly(reason), _, _) => eprintln!("{summary}; gate={reason}; {see}"),
        (Gate::Reclaim, _, _) => eprintln!("{summary}; gate=Reclaim; nothing to drop"),
    }
    Ok(())
}

/// Best-effort: a failed sweep never fails the fixture that triggered it.
pub(super) async fn reclaim_once_per_process(admin_pool: &PgPool, label: &str) {
    if SWEPT_THIS_PROCESS.swap(true, Ordering::SeqCst) {
        return;
    }
    if let Err(error) = sweep(admin_pool, label).await {
        eprintln!("pg test db reclaim aborted: {error}");
        tracing::warn!(label, error, "postgres test db reclaim sweep failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read};
    use std::path::{Path, PathBuf};
    use std::process::{Child, ChildStdout, Command, Stdio};

    const LABEL: &str = "db::postgres reclaim tests";
    const CHILD_ENV: &str = "AGENTDESK_TEST_PG_RECLAIM_CHILD";
    const CHILD_TEST: &str = "db::postgres::test_db_reclaim::tests::pg_reclaim_fresh_process_child";
    const HOST: &str = "reclaim-host";

    fn owner(host: &str, pid: u32) -> Owner {
        Owner {
            host: host.to_string(),
            pid,
            start: Some(7),
            lstart: None,
        }
    }

    fn row(name: &str, comment: Option<String>) -> FixtureRow {
        FixtureRow {
            name: name.to_string(),
            oid: 1,
            owner_role: 10,
            comment,
            is_template: false,
        }
    }

    fn v2(created_at: i64, owner: &Owner) -> Option<String> {
        Some(marker_text(created_at, owner))
    }

    fn verdict_of(row: &FixtureRow, probe: impl Fn(&Owner) -> ProcessIdentityProbe) -> Verdict {
        classify_row(row, Some(HOST), &probe).verdict
    }

    #[test]
    fn reclaim_classify_owner_identity() {
        let gone = |_: &Owner| ProcessIdentityProbe::GoneOrReused;
        let unreachable = |_: &Owner| -> ProcessIdentityProbe { panic!("must not probe") };
        // Death is proven by the probe alone; the creation time does not matter.
        for created_at in [0, 4_000_000_000] {
            let dead = row("fx_dead", v2(created_at, &owner(HOST, 41)));
            assert_eq!(verdict_of(&dead, gone), Verdict::OwnerDead);
        }
        let marked = row("fx", v2(0, &owner(HOST, 41)));
        assert_eq!(
            verdict_of(&marked, |_| ProcessIdentityProbe::Same),
            Verdict::OwnerAlive
        );
        assert_eq!(
            verdict_of(&marked, |_| ProcessIdentityProbe::ProbeError),
            Verdict::OwnerUnproven
        );
        let foreign = row("fx", v2(0, &owner("other-host", 41)));
        assert_eq!(verdict_of(&foreign, unreachable), Verdict::OwnerUnproven);
        let unknown_host = row("fx", v2(0, &owner("-", 41)));
        assert_eq!(
            verdict_of(&unknown_host, unreachable),
            Verdict::OwnerUnproven
        );
        assert_eq!(
            classify_row(&marked, None, &unreachable).verdict,
            Verdict::OwnerUnproven
        );
        let v1 = row("fx", Some(format!("{MARKER_PREFIX}1")));
        assert_eq!(verdict_of(&v1, unreachable), Verdict::OwnerUnproven);
        let pending = row(&pending_database_name(1), None);
        assert_eq!(verdict_of(&pending, unreachable), Verdict::OwnerUnproven);
        for comment in [
            format!("{MARKER_PREFIX}1 extra"),
            format!("x {MARKER_PREFIX}1"),
            format!("{MARKER_PREFIX}1 host=a b pid=4 start=- lstart=-"),
            format!("{MARKER_PREFIX}1 host=h pid=0 start=- lstart=-"),
            format!("{MARKER_PREFIX}1 host=h pid=4294967295 start=- lstart=-"),
        ] {
            assert_eq!(
                verdict_of(&row("fx", Some(comment.clone())), unreachable),
                Verdict::Malformed,
                "{comment}"
            );
        }
        assert_eq!(
            verdict_of(&row("agentdesk_pending_1_notuuid", None), unreachable),
            Verdict::Malformed
        );
    }

    /// The real identity probe on this process: same start is alive, another start is a reused pid.
    #[cfg(unix)]
    #[test]
    fn reclaim_classify_real_process_identity() {
        let me = Owner {
            host: HOST.to_string(),
            ..own_owner().clone()
        };
        let start = me.start.or(me.lstart).expect("own start time is readable");
        let alive = row("fx", v2(0, &me));
        assert_eq!(verdict_of(&alive, probe_owner), Verdict::OwnerAlive);
        let reused = Owner {
            start: me.start.map(|_| start + 1),
            lstart: me.lstart.map(|value| value + 1),
            ..me
        };
        assert_eq!(
            verdict_of(&row("fx", v2(0, &reused)), probe_owner),
            Verdict::OwnerDead
        );
    }

    #[test]
    fn reclaim_protected_names_abort_sweep() {
        let gone = |_: &Owner| ProcessIdentityProbe::GoneOrReused;
        for name in PROTECTED_DATABASE_NAMES {
            let rows = [
                row("fx_dead", v2(0, &owner(HOST, 41))),
                row(name, v2(0, &owner(HOST, 41))),
            ];
            assert!(classify(&rows, Some(HOST), &gone).is_err(), "{name}");
        }
        let template = FixtureRow {
            is_template: true,
            ..row("fx_template", v2(0, &owner(HOST, 41)))
        };
        assert!(classify(&[template], Some(HOST), &gone).is_err());
    }

    #[test]
    fn reclaim_gate_decision() {
        let gate =
            |sysid: Option<u64>, operational: bool, opt_in: Option<&str>, deny: Option<&str>| {
                decide_gate(&GateInputs {
                    sysid,
                    operational_present: operational,
                    opt_in,
                    deny,
                })
            };
        let canonical = CANONICAL_SERVER_SYSIDS[0];
        let canonical_text = canonical.to_string();
        assert_eq!(gate(Some(42), false, Some("42"), None), Gate::Reclaim);
        assert_eq!(
            gate(Some(42), false, Some(" 42 "), Some("7,8")),
            Gate::Reclaim
        );
        let refused = [
            (
                gate(None, false, Some("42"), None),
                "ServerIdentityUnreadable",
            ),
            (
                gate(Some(canonical), false, Some(&canonical_text), None),
                "DeniedServer",
            ),
            (
                gate(Some(42), false, Some("42"), Some("7, 42")),
                "DeniedServer",
            ),
            (
                gate(Some(42), false, Some("42"), Some("7 42")),
                "DenyListMalformed",
            ),
            (
                gate(Some(42), false, Some("42"), Some("7,")),
                "DenyListMalformed",
            ),
            (
                gate(Some(42), true, Some("42"), None),
                "OperationalDatabasePresent",
            ),
            (gate(Some(42), false, None, None), "NotOptedIn"),
            (gate(Some(42), false, Some("42 43"), None), "OptInMalformed"),
            (gate(Some(42), false, Some("42,43"), None), "OptInMalformed"),
            (
                gate(Some(42), false, Some("123456789012345678901"), None),
                "OptInMalformed",
            ),
            (gate(Some(42), false, Some("43"), None), "OptInMismatch"),
        ];
        for (index, (decided, reason)) in refused.into_iter().enumerate() {
            assert_eq!(decided, Gate::ListOnly(reason), "case {index}");
        }
    }

    /// The marker matches `FakeBackend`'s unchanged row; `created_at` only orders the plan.
    fn dead(name: &str, created_at: i64) -> Classified {
        Classified {
            row: row(name, v2(0, &owner(HOST, 41))),
            created_at: Some(created_at),
            owner: Some(owner(HOST, 41)),
            verdict: Verdict::OwnerDead,
        }
    }

    #[test]
    fn reclaim_plan_caps_per_process_oldest_first() {
        let mut classified: Vec<Classified> = (0..20)
            .map(|index| dead(&format!("fx_{index:02}"), 100 + (index * 7) % 20))
            .collect();
        classified.push(Classified {
            verdict: Verdict::OwnerUnproven,
            ..dead("fx_unproven", 0)
        });
        let (plan, deferred) = plan_drops(&classified);
        let created: Vec<i64> = plan.iter().filter_map(|entry| entry.created_at).collect();
        assert_eq!(created, (100..108).collect::<Vec<_>>());
        assert_eq!(plan.len(), RECLAIM_MAX_DROPS_PER_PROCESS);
        assert_eq!(deferred, 12);
        assert!(summary(Some(1), &classified).contains("per-process"));
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Step {
        Write,
        Flush,
        Sync,
    }

    /// Fails `step` while recording line number `at` (0-based).
    struct FaultyOut {
        fail: Option<(Step, usize)>,
        recorded: std::cell::Cell<usize>,
    }

    impl FaultyOut {
        fn check(&self, step: Step) -> std::io::Result<()> {
            match self.fail {
                Some((failing, at)) if failing == step && at == self.recorded.get() => {
                    Err(std::io::Error::other("injected"))
                }
                _ => Ok(()),
            }
        }
    }

    impl Write for FaultyOut {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.check(Step::Write).map(|()| buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.check(Step::Flush)
        }
    }

    impl SyncData for FaultyOut {
        fn sync_data(&self) -> std::io::Result<()> {
            self.check(Step::Sync)?;
            self.recorded.set(self.recorded.get() + 1);
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeBackend {
        changed: std::collections::HashMap<String, Option<CurrentRow>>,
        dropped: Vec<String>,
    }

    impl ReclaimBackend for FakeBackend {
        async fn current(&mut self, name: &str) -> Result<Option<CurrentRow>, String> {
            if let Some(changed) = self.changed.remove(name) {
                return Ok(changed);
            }
            let planned = row(name, v2(0, &owner(HOST, 41)));
            Ok(Some(CurrentRow {
                oid: planned.oid,
                owner_role: planned.owner_role,
                comment: planned.comment,
                is_template: false,
                has_session: false,
            }))
        }
        async fn drop_database(&mut self, name: &str) -> Result<(), String> {
            self.dropped.push(name.to_string());
            Ok(())
        }
    }

    async fn run_execute(
        plan: &[Classified],
        backend: &mut FakeBackend,
        fail: Option<(Step, usize)>,
        probe: impl Fn(&Owner) -> ProcessIdentityProbe,
    ) -> Outcome {
        let mut log = AuditLog {
            out: FaultyOut {
                fail,
                recorded: std::cell::Cell::new(0),
            },
        };
        let plan: Vec<&Classified> = plan.iter().collect();
        execute(
            &plan,
            1,
            &mut log,
            backend,
            Some(HOST),
            &probe,
            RECLAIM_DROP_BUDGET,
        )
        .await
    }

    #[tokio::test]
    async fn reclaim_execute_stops_on_log_failure() {
        let plan = [dead("fx_a", 1), dead("fx_b", 2), dead("fx_c", 3)];
        let gone = |_: &Owner| ProcessIdentityProbe::GoneOrReused;
        let mut clean = FakeBackend::default();
        let outcome = run_execute(&plan, &mut clean, None, gone).await;
        assert_eq!(clean.dropped, ["fx_a", "fx_b", "fx_c"]);
        assert_eq!(outcome.stopped, None);
        // Lines alternate intent, result per database: line 2 is fx_b's intent, line 1 fx_a's result.
        for step in [Step::Write, Step::Flush, Step::Sync] {
            let mut backend = FakeBackend::default();
            let outcome = run_execute(&plan, &mut backend, Some((step, 2)), gone).await;
            assert_eq!(
                backend.dropped,
                ["fx_a"],
                "intent failure must precede the DROP"
            );
            assert!(outcome.stopped.is_some());
            let mut backend = FakeBackend::default();
            let outcome = run_execute(&plan, &mut backend, Some((step, 1)), gone).await;
            assert_eq!(
                backend.dropped,
                ["fx_a"],
                "result failure stops later drops"
            );
            assert!(outcome.stopped.is_some());
        }
    }

    #[tokio::test]
    async fn reclaim_execute_rechecks_before_drop() {
        let names = [
            "fx_gone",
            "fx_oid",
            "fx_comment",
            "fx_role",
            "fx_session",
            "fx_template",
            "fx_alive",
            "fx_ok",
        ];
        let plan: Vec<Classified> = names
            .iter()
            .enumerate()
            .map(|(index, name)| {
                let pid = if *name == "fx_alive" { 99 } else { 41 };
                Classified {
                    row: row(name, v2(0, &owner(HOST, pid))),
                    created_at: Some(i64::try_from(index).unwrap_or_default()),
                    owner: Some(owner(HOST, pid)),
                    verdict: Verdict::OwnerDead,
                }
            })
            .collect();
        let base = |_: &str| CurrentRow {
            oid: 1,
            owner_role: 10,
            comment: v2(0, &owner(HOST, 41)),
            is_template: false,
            has_session: false,
        };
        let mut backend = FakeBackend::default();
        backend.changed.insert("fx_gone".into(), None);
        backend
            .changed
            .insert("fx_oid".into(), Some(CurrentRow { oid: 2, ..base("") }));
        backend.changed.insert(
            "fx_comment".into(),
            Some(CurrentRow {
                comment: v2(5, &owner(HOST, 41)),
                ..base("")
            }),
        );
        backend.changed.insert(
            "fx_role".into(),
            Some(CurrentRow {
                owner_role: 11,
                ..base("")
            }),
        );
        backend.changed.insert(
            "fx_session".into(),
            Some(CurrentRow {
                has_session: true,
                ..base("")
            }),
        );
        backend.changed.insert(
            "fx_template".into(),
            Some(CurrentRow {
                is_template: true,
                ..base("")
            }),
        );
        backend.changed.insert(
            "fx_alive".into(),
            Some(CurrentRow {
                comment: v2(0, &owner(HOST, 99)),
                ..base("")
            }),
        );
        let probe = |owner: &Owner| {
            if owner.pid == 99 {
                ProcessIdentityProbe::Same
            } else {
                ProcessIdentityProbe::GoneOrReused
            }
        };
        let outcome = run_execute(&plan, &mut backend, None, probe).await;
        assert_eq!(backend.dropped, ["fx_ok"]);
        assert_eq!(outcome.kept, names[..7].to_vec());
        assert_eq!(outcome.stopped, None);
    }

    fn require_pg() -> bool {
        std::env::var("AGENTDESK_REQUIRE_PG").ok().as_deref() == Some("1")
    }

    fn postgres_bin_dir() -> Option<PathBuf> {
        let has_initdb = |dir: &Path| dir.join("initdb").is_file();
        if let Some(found) = std::env::var_os("PATH")
            .and_then(|path| std::env::split_paths(&path).find(|dir| has_initdb(dir)))
        {
            return Some(found);
        }
        let from_pg_config = Command::new("pg_config")
            .arg("--bindir")
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| PathBuf::from(String::from_utf8_lossy(&output.stdout).trim()));
        if let Some(dir) = from_pg_config.filter(|dir| has_initdb(dir)) {
            return Some(dir);
        }
        let mut versions: Vec<PathBuf> = std::fs::read_dir("/usr/lib/postgresql")
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path().join("bin"))
            .filter(|dir| has_initdb(dir))
            .collect();
        versions.sort();
        versions.pop()
    }

    fn run_tool(bin: &Path, tool: &str, args: &[&str]) {
        let output = Command::new(bin.join(tool))
            .args(args)
            .env("LC_ALL", "C")
            .env("LANG", "C")
            .output()
            .unwrap_or_else(|error| panic!("{tool} failed to start: {error}"));
        assert!(
            output.status.success(),
            "{tool} {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// A throwaway server this test initialised itself; Drop stops and removes exactly this cluster.
    struct OwnCluster {
        bin: PathBuf,
        dir: PathBuf,
        base: String,
    }

    impl OwnCluster {
        fn start() -> Option<Self> {
            let Some(bin) = postgres_bin_dir() else {
                assert!(
                    !require_pg(),
                    "AGENTDESK_REQUIRE_PG=1 but initdb is not installed"
                );
                println!("NOT_RUN: initdb not found; own-cluster reclaim test did nothing");
                return None;
            };
            let dir = std::env::temp_dir()
                .join("agentdesk-pg-reclaim-test")
                .join(format!(
                    "{}-{}",
                    std::process::id(),
                    uuid::Uuid::new_v4().simple()
                ));
            std::fs::create_dir_all(&dir).expect("create cluster dir");
            let port = std::net::TcpListener::bind("127.0.0.1:0")
                .and_then(|listener| listener.local_addr())
                .expect("pick a free port")
                .port();
            let cluster = Self {
                bin,
                dir,
                base: format!("postgres://postgres@127.0.0.1:{port}"),
            };
            let data = cluster.dir.join("data");
            let data = data.to_str().expect("utf-8 cluster path").to_string();
            run_tool(
                &cluster.bin,
                "initdb",
                &[
                    "-D",
                    &data,
                    "-U",
                    "postgres",
                    "-A",
                    "trust",
                    "-E",
                    "UTF8",
                    "--locale=C",
                ],
            );
            let options =
                format!("-p {port} -c listen_addresses=127.0.0.1 -c unix_socket_directories=''");
            let log = cluster.dir.join("server.log");
            run_tool(
                &cluster.bin,
                "pg_ctl",
                &[
                    "-D",
                    &data,
                    "-o",
                    &options,
                    "-l",
                    log.to_str().expect("utf-8 log path"),
                    "-w",
                    "start",
                ],
            );
            Some(cluster)
        }
    }

    impl Drop for OwnCluster {
        fn drop(&mut self) {
            let data = self.dir.join("data");
            let _ = Command::new(self.bin.join("pg_ctl"))
                .args([
                    "-D".as_ref(),
                    data.as_os_str(),
                    "-m".as_ref(),
                    "immediate".as_ref(),
                    "-w".as_ref(),
                    "stop".as_ref(),
                ])
                .env("LC_ALL", "C")
                .output();
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// Routes this thread's fixture connections to the own cluster for the test's lifetime.
    struct BaseOverride(Option<Option<String>>);

    impl Drop for BaseOverride {
        fn drop(&mut self) {
            let previous = self.0.take();
            crate::db::postgres::FIXTURE_BASE_OVERRIDE.with(|slot| {
                slot.replace(previous);
            });
        }
    }

    struct Env {
        admin: PgPool,
        _base: BaseOverride,
        cluster: OwnCluster,
    }

    async fn own_env() -> Option<Env> {
        let cluster = OwnCluster::start()?;
        let previous = crate::db::postgres::FIXTURE_BASE_OVERRIDE
            .with(|slot| slot.replace(Some(Some(cluster.base.clone()))));
        let base = BaseOverride(previous);
        let admin =
            crate::db::postgres::connect_test_pool(&format!("{}/postgres", cluster.base), LABEL)
                .await
                .expect("connect own cluster");
        Some(Env {
            admin,
            _base: base,
            cluster,
        })
    }

    impl Env {
        async fn rows(&self) -> Vec<FixtureRow> {
            list_fixture_rows(&self.admin)
                .await
                .expect("list fixture rows")
        }

        async fn row(&self, name: &str) -> Option<FixtureRow> {
            self.rows().await.into_iter().find(|row| row.name == name)
        }

        async fn verdict(&self, name: &str) -> Verdict {
            let row = self
                .row(name)
                .await
                .unwrap_or_else(|| panic!("{name} not listed"));
            classify_row(&row, local_host(), &probe_owner).verdict
        }

        async fn exists(&self, name: &str) -> bool {
            sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname = $1)",
            )
            .bind(name)
            .fetch_one(&self.admin)
            .await
            .expect("query pg_database")
        }

        async fn sql(&self, statement: &str) {
            sqlx::query(statement)
                .execute(&self.admin)
                .await
                .unwrap_or_else(|error| panic!("{statement}: {error}"));
        }

        async fn sysid(&self) -> String {
            sqlx::query_scalar::<_, String>(
                "SELECT system_identifier::text FROM pg_control_system()",
            )
            .fetch_one(&self.admin)
            .await
            .expect("read sysid")
        }

        /// A database whose v2 owner was a child this test SIGKILLed and reaped.
        async fn orphan(&self) -> String {
            let mut child = ChildRun::spawn(&self.cluster, "hold", &[]);
            let name = child.line("RECLAIM_CHILD READY ");
            child.kill();
            name
        }
    }

    struct ChildRun {
        child: Child,
        stdout: BufReader<ChildStdout>,
        stderr: PathBuf,
    }

    impl ChildRun {
        fn spawn(cluster: &OwnCluster, mode: &str, env: &[(&str, &str)]) -> Self {
            let stderr = cluster
                .dir
                .join(format!("child-{}.stderr", uuid::Uuid::new_v4().simple()));
            let mut child = Command::new(std::env::current_exe().expect("test binary"))
                .args([
                    "--ignored",
                    "--exact",
                    CHILD_TEST,
                    "--test-threads=1",
                    "--nocapture",
                ])
                .env(CHILD_ENV, mode)
                .env("POSTGRES_TEST_DATABASE_URL_BASE", &cluster.base)
                .env("POSTGRES_TEST_ADMIN_DB", "postgres")
                .env_remove(OPT_IN_ENV)
                .env_remove(DENY_ENV)
                .env_remove(LOG_ENV)
                .env_remove(PAUSE_ENV)
                .envs(env.iter().copied())
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(std::fs::File::create(&stderr).expect("child stderr file"))
                .spawn()
                .expect("spawn child test process");
            let stdout = BufReader::new(child.stdout.take().expect("child stdout"));
            Self {
                child,
                stdout,
                stderr,
            }
        }

        fn stderr(&self) -> String {
            std::fs::read_to_string(&self.stderr).unwrap_or_default()
        }

        /// The rest of the first stdout line starting with `prefix`.
        fn line(&mut self, prefix: &str) -> String {
            let mut line = String::new();
            loop {
                line.clear();
                let read = self.stdout.read_line(&mut line).expect("read child stdout");
                assert!(
                    read > 0,
                    "child exited before {prefix:?}:\n{}",
                    self.stderr()
                );
                // libtest prints `test <name> ... ` without a newline before the child's own output.
                if let Some(at) = line.find(prefix) {
                    return line[at + prefix.len()..].trim().to_string();
                }
            }
        }

        fn kill(mut self) {
            self.child.kill().expect("SIGKILL child");
            let status = self.child.wait().expect("reap child");
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                assert_eq!(status.signal(), Some(9), "{status:?}");
            }
            #[cfg(not(unix))]
            let _ = status;
        }

        /// Lets a holding child finish; returns its stderr.
        fn finish(mut self) -> String {
            if let Some(mut stdin) = self.child.stdin.take() {
                let _ = stdin.write_all(b"\n");
            }
            let mut rest = String::new();
            let _ = self.stdout.read_to_string(&mut rest);
            let status = self.child.wait().expect("wait child");
            let stderr = self.stderr();
            assert!(status.success(), "child failed:\n{rest}\n{stderr}");
            assert!(
                rest.contains("test result: ok. 1 passed"),
                "child test did not run:\n{rest}"
            );
            stderr
        }
    }

    /// A failing test must not leave a holding child behind.
    impl Drop for ChildRun {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    fn fresh_name(tag: &str) -> String {
        format!("agentdesk_reclaim_{tag}_{}", uuid::Uuid::new_v4().simple())
    }

    async fn drop_all(env: &Env, names: &[String]) {
        for name in names {
            env.sql(&format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"))
                .await;
        }
    }

    #[tokio::test]
    async fn pg_reclaim_never_targets_unmarked_databases() {
        let Some(env) = own_env().await else { return };
        let plain = fresh_name("plain");
        let foreign = fresh_name("foreign");
        let v1 = fresh_name("v1");
        let malformed = fresh_name("malformed");
        let pending = pending_database_name(1);
        let names = [
            plain.clone(),
            foreign.clone(),
            v1.clone(),
            malformed.clone(),
            pending.clone(),
        ];
        for name in &names {
            env.sql(&format!("CREATE DATABASE \"{name}\"")).await;
        }
        env.sql(&format!(
            "COMMENT ON DATABASE \"{foreign}\" IS 'production data'"
        ))
        .await;
        env.sql(&format!(
            "COMMENT ON DATABASE \"{v1}\" IS '{MARKER_PREFIX}1'"
        ))
        .await;
        env.sql(&format!(
            "COMMENT ON DATABASE \"{malformed}\" IS '{MARKER_PREFIX}1 host=h pid=1 start=- lstart=- x'"
        ))
        .await;

        let rows = env.rows().await;
        let classified = classify(&rows, local_host(), &probe_owner).expect("classify");
        let verdict = |name: &str| {
            classified
                .iter()
                .find(|entry| entry.row.name == name)
                .map(|entry| entry.verdict)
        };
        let (plan, _) = plan_drops(&classified);
        drop_all(&env, &names).await;

        assert_eq!(verdict(&plain), None);
        assert_eq!(verdict(&foreign), None);
        assert_eq!(verdict(&malformed), Some(Verdict::Malformed));
        // Old but ownerless: kept for the manual procedure, never planned.
        assert_eq!(verdict(&v1), Some(Verdict::OwnerUnproven));
        assert_eq!(verdict(&pending), Some(Verdict::OwnerUnproven));
        assert!(plan.is_empty());
    }

    #[tokio::test]
    async fn pg_reclaim_dead_owner_young_database_is_reclaimed_immediately() {
        let Some(env) = own_env().await else { return };
        let mut child = ChildRun::spawn(&env.cluster, "hold", &[]);
        let child_pid = child.child.id();
        let name = child.line("RECLAIM_CHILD READY ");
        let row = env.row(&name).await.expect("fixture listed");
        let (created_at, owner) = row
            .comment
            .as_deref()
            .and_then(parse_marker)
            .expect("v2 marker");
        let now = server_now_unix(&env.admin).await.expect("server clock");
        assert!(
            now.abs_diff(created_at) <= 60,
            "not young: {created_at} vs {now}"
        );
        assert_eq!(owner.map(|owner| owner.pid), Some(child_pid));
        assert_eq!(env.verdict(&name).await, Verdict::OwnerAlive);

        child.kill();
        let rows = env.rows().await;
        let classified = classify(&rows, local_host(), &probe_owner).expect("classify");
        let (plan, _) = plan_drops(&classified);
        assert_eq!(
            plan.iter()
                .map(|entry| entry.row.name.as_str())
                .collect::<Vec<_>>(),
            [name.as_str()]
        );
        let log_path = env.cluster.dir.join("audit.jsonl");
        let mut log = AuditLog {
            out: std::fs::File::create(&log_path).expect("audit log"),
        };
        let mut backend = PgBackend {
            pool: &env.admin,
            label: LABEL,
        };
        let outcome = execute(
            &plan,
            1,
            &mut log,
            &mut backend,
            local_host(),
            &probe_owner,
            RECLAIM_DROP_BUDGET,
        )
        .await;

        assert_eq!(outcome.dropped, [name.clone()]);
        assert!(!env.exists(&name).await);
        let lines = std::fs::read_to_string(&log_path).expect("read audit log");
        let phases: Vec<String> = lines
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("json line"))
            .filter(|entry| entry["datname"] == name.as_str())
            .map(|entry| format!("{}:{}", entry["phase"], entry["result"]))
            .collect();
        assert_eq!(phases, ["\"intent\":\"drop\"", "\"result\":\"dropped\""]);
    }

    #[tokio::test]
    async fn pg_reclaim_live_owner_old_database_is_kept() {
        let Some(env) = own_env().await else { return };
        let mut child = ChildRun::spawn(&env.cluster, "hold_old", &[]);
        let name = child.line("RECLAIM_CHILD READY ");
        let row = env.row(&name).await.expect("fixture listed");
        let (created_at, _) = row
            .comment
            .as_deref()
            .and_then(parse_marker)
            .expect("marker");
        let now = server_now_unix(&env.admin).await.expect("server clock");
        assert!(
            now - created_at >= 6 * 24 * 60 * 60,
            "not old: {created_at} vs {now}"
        );

        let classified = classify(&env.rows().await, local_host(), &probe_owner).expect("classify");
        let verdict = classified
            .iter()
            .find(|entry| entry.row.name == name)
            .map(|entry| entry.verdict);
        assert_eq!(verdict, Some(Verdict::OwnerAlive));
        assert!(plan_drops(&classified).0.is_empty());
        child.finish();
        assert!(
            !env.exists(&name).await,
            "child did not drop its own fixture"
        );
    }

    #[tokio::test]
    async fn pg_reclaim_create_boundaries_protect_live_owner() {
        let Some(env) = own_env().await else { return };
        for (stage, after_kill) in [
            ("after_create", Verdict::OwnerUnproven),
            ("after_comment", Verdict::OwnerDead),
            ("after_rename", Verdict::OwnerDead),
        ] {
            let mut child = ChildRun::spawn(&env.cluster, "hold", &[(PAUSE_ENV, stage)]);
            let paused = child.line(&format!("RECLAIM_CHILD PAUSED {stage} "));
            let alive = env.verdict(&paused).await;
            child.kill();
            let dead = env.verdict(&paused).await;
            drop_all(&env, &[paused.clone()]).await;
            let while_alive = if stage == "after_create" {
                Verdict::OwnerUnproven
            } else {
                Verdict::OwnerAlive
            };
            assert_eq!(alive, while_alive, "{stage} {paused}");
            assert_eq!(dead, after_kill, "{stage} {paused}");
        }
    }

    /// The sweep inside the real `create_test_database` of a fresh process, refusal by refusal.
    #[tokio::test]
    async fn pg_reclaim_entrypoint_gate_wiring() {
        let Some(env) = own_env().await else { return };
        let orphan = env.orphan().await;
        let sysid = env.sysid().await;
        let directory = env.cluster.dir.to_str().expect("utf-8 dir").to_string();
        let log_path = env.cluster.dir.join("wiring.jsonl");
        let log = log_path.to_str().expect("utf-8 log path").to_string();
        let operational = "memento";

        let refusals: [(&str, Vec<(&str, &str)>); 5] = [
            ("NotOptedIn", vec![]),
            ("OptInMismatch", vec![(OPT_IN_ENV, "1")]),
            (
                "DeniedServer",
                vec![(OPT_IN_ENV, &sysid), (DENY_ENV, &sysid)],
            ),
            ("OperationalDatabasePresent", vec![(OPT_IN_ENV, &sysid)]),
            (
                "LogUnavailable",
                vec![(OPT_IN_ENV, &sysid), (LOG_ENV, &directory)],
            ),
        ];
        for (reason, child_env) in refusals {
            if reason == "OperationalDatabasePresent" {
                env.sql(&format!("CREATE DATABASE {operational}")).await;
            }
            let stderr = ChildRun::spawn(&env.cluster, "sweep", &child_env).finish();
            if reason == "OperationalDatabasePresent" {
                env.sql(&format!("DROP DATABASE {operational}")).await;
            }
            assert!(
                stderr.contains(&format!("gate={reason}")),
                "{reason}:\n{stderr}"
            );
            assert!(env.exists(&orphan).await, "{reason} dropped {orphan}");
        }

        let stderr = ChildRun::spawn(
            &env.cluster,
            "sweep",
            &[(OPT_IN_ENV, &sysid), (LOG_ENV, &log)],
        )
        .finish();
        assert!(stderr.contains("gate=Reclaim"), "{stderr}");
        assert!(
            !env.exists(&orphan).await,
            "opted-in sweep kept {orphan}:\n{stderr}"
        );
        let lines = std::fs::read_to_string(&log_path).expect("read audit log");
        assert!(
            lines.contains(&format!("\"datname\":\"{orphan}\"")),
            "{lines}"
        );
    }

    #[tokio::test]
    async fn pg_reclaim_same_name_recreated_after_scan_is_kept() {
        let Some(env) = own_env().await else { return };
        let orphan = env.orphan().await;
        let classified = classify(&env.rows().await, local_host(), &probe_owner).expect("classify");
        let (plan, _) = plan_drops(&classified);
        assert_eq!(plan.len(), 1);
        // Barrier: the planned database is replaced by a live fixture under the same name.
        env.sql(&format!("DROP DATABASE \"{orphan}\"")).await;
        env.sql(&format!("CREATE DATABASE \"{orphan}\"")).await;
        let now = server_now_unix(&env.admin).await.expect("server clock");
        mark_test_database(&env.admin, &orphan, now, LABEL)
            .await
            .expect("mark");

        let mut log = AuditLog {
            out: std::fs::File::create(env.cluster.dir.join("same-name.jsonl")).expect("log"),
        };
        let mut backend = PgBackend {
            pool: &env.admin,
            label: LABEL,
        };
        let outcome = execute(
            &plan,
            1,
            &mut log,
            &mut backend,
            local_host(),
            &probe_owner,
            RECLAIM_DROP_BUDGET,
        )
        .await;
        let survived = env.exists(&orphan).await;
        drop_all(&env, &[orphan.clone()]).await;
        assert!(outcome.dropped.is_empty(), "{outcome:?}");
        assert!(survived);
    }

    #[tokio::test]
    async fn pg_reclaim_skips_database_with_active_session() {
        let Some(env) = own_env().await else { return };
        let orphan = env.orphan().await;
        let session = crate::db::postgres::connect_test_pool(
            &format!("{}/{orphan}", env.cluster.base),
            LABEL,
        )
        .await
        .expect("connect orphan");
        sqlx::query("SELECT 1")
            .execute(&session)
            .await
            .expect("open session");
        let listed = env.row(&orphan).await.is_some();
        session.close().await;
        let listed_after_close = env.row(&orphan).await.is_some();
        drop_all(&env, &[orphan.clone()]).await;
        assert!(!listed, "{orphan} listed while a session is open");
        assert!(listed_after_close);
    }

    /// Driven by the own-cluster tests above; without its env it does nothing.
    #[tokio::test]
    #[ignore = "helper subprocess for the own-cluster reclaim tests"]
    async fn pg_reclaim_fresh_process_child() {
        let Ok(mode) = std::env::var(CHILD_ENV) else {
            return;
        };
        let base = crate::db::postgres::postgres_test_database_url_base().expect("child base");
        let admin_url = format!("{base}/postgres");
        let name = fresh_name("child");
        crate::db::postgres::create_test_database(&admin_url, &name, LABEL)
            .await
            .expect("child create");
        if mode == "hold_old" {
            let admin = crate::db::postgres::connect_test_pool(&admin_url, LABEL)
                .await
                .expect("child admin");
            let now = server_now_unix(&admin).await.expect("server clock");
            mark_test_database(&admin, &name, now - 7 * 24 * 60 * 60, LABEL)
                .await
                .expect("backdate own marker");
            admin.close().await;
        }
        println!("RECLAIM_CHILD READY {name}");
        if mode != "sweep" {
            let _ = std::io::stdin().read_line(&mut String::new());
        }
        crate::db::postgres::drop_test_database(&admin_url, &name, LABEL)
            .await
            .expect("child drop");
    }

    /// Prints what the sweep would decide on the configured fixture server; drops nothing.
    /// `cargo test --lib pg_reclaim_dry_run -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn pg_reclaim_dry_run() {
        let Some(base) = crate::db::postgres::postgres_test_database_url_base() else {
            println!("POSTGRES_TEST_DATABASE_URL_BASE unset");
            return;
        };
        let admin = crate::db::postgres::connect_test_pool(&format!("{base}/postgres"), LABEL)
            .await
            .expect("connect admin pool");
        let (sysid, operational_present) = observe_server(&admin).await.expect("observe");
        let (opt_in, deny) = (env_value(OPT_IN_ENV), env_value(DENY_ENV));
        let gate = decide_gate(&GateInputs {
            sysid,
            operational_present,
            opt_in: opt_in.as_deref(),
            deny: deny.as_deref(),
        });
        let rows = list_fixture_rows(&admin).await.expect("list");
        let classified = match classify(&rows, local_host(), &probe_owner) {
            Ok(classified) => classified,
            Err(error) => panic!("sweep would abort: {error}"),
        };
        println!("{}; gate={gate:?}", summary(sysid, &classified));
        for entry in &classified {
            println!(
                "  {:?} {} oid={} created_at={:?} owner={:?}",
                entry.verdict, entry.row.name, entry.row.oid, entry.created_at, entry.owner
            );
        }
    }
}
