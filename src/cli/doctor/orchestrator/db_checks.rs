use super::{
    Check, CheckGroup, FixSafety, SecurityExposure, Value, config, dcserver_log_hint, json,
    stale_zero_byte_db_candidates,
};

pub(super) fn check_postgres_connection(cfg: &config::Config) -> Check {
    let summary = crate::db::postgres::database_summary(cfg);
    if !crate::db::postgres::database_enabled(cfg) {
        return Check::ok(
            "postgres_connection",
            CheckGroup::Core,
            "PostgreSQL",
            "disabled",
        )
        .with_expected_actual("postgres bootstrap configured", "disabled");
    }

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            return Check::fail(
                "postgres_connection",
                CheckGroup::Core,
                "PostgreSQL",
                format!("{summary} — runtime init failed"),
                "postgres 연결 검증용 async runtime 생성에 실패했습니다.",
            )
            .with_expected_actual(
                "postgres check runtime initializes",
                format!("runtime build failed: {error}"),
            );
        }
    };

    match runtime.block_on(crate::db::postgres::connect(cfg)) {
        Ok(Some(pool)) => {
            let migration_status = runtime.block_on(crate::db::postgres::migration_status(&pool));
            let checksum_mismatches = runtime
                .block_on(crate::db::postgres::applied_migration_checksum_mismatch_details(&pool));
            drop(pool);
            match (migration_status, checksum_mismatches) {
                (Ok(status), Ok(checksum_mismatches))
                    if postgres_migration_status_is_healthy(&status, &checksum_mismatches) =>
                {
                    let pending = status.pending_versions.len();
                    Check::ok(
                        "postgres_connection",
                        CheckGroup::Core,
                        "PostgreSQL",
                        format!(
                            "{summary} — ok; applied={} resolved={} pending={pending}",
                            status.applied.len(),
                            status.resolved_versions.len(),
                        ),
                    )
                    .with_subsystem("postgres")
                    .with_evidence(json!({
                        "applied_count": status.applied.len(),
                        "resolved_count": status.resolved_versions.len(),
                        "pending_versions": status.pending_versions,
                        "checksum_mismatches": checksum_mismatch_evidence(&checksum_mismatches),
                    }))
                    .with_expected_actual("postgres connection and migration metadata readable", "ok")
                }
                (Ok(status), Ok(checksum_mismatches)) => Check::fail(
                    "postgres_connection",
                    CheckGroup::Core,
                    "PostgreSQL",
                    format!(
                        "{summary} — migration drift: missing_from_resolved={:?} unsuccessful={:?} checksum_mismatches={}",
                        status.missing_from_resolved,
                        unsuccessful_migration_versions(&status),
                        format_checksum_mismatches(&checksum_mismatches)
                    ),
                    "Postgres _sqlx_migrations contains drift or unsuccessful migration records.",
                )
                .with_subsystem("postgres")
                .with_fix_safety(FixSafety::NotFixable)
                .with_security_exposure(SecurityExposure::OperationalMetadata)
                .with_evidence(json!({
                    "applied_count": status.applied.len(),
                    "resolved_count": status.resolved_versions.len(),
                    "missing_from_resolved": status.missing_from_resolved,
                    "unsuccessful_versions": unsuccessful_migration_versions(&status),
                    "pending_versions": status.pending_versions,
                    "checksum_mismatches": checksum_mismatch_evidence(&checksum_mismatches),
                }))
                .with_expected_actual(
                    "applied migrations all exist in resolved migrations, succeeded, and checksum-matched",
                    "migration drift, checksum mismatch, or unsuccessful migration",
                ),
                (Err(error), _) => Check::fail(
                    "postgres_connection",
                    CheckGroup::Core,
                    "PostgreSQL",
                    format!("{summary} — migration metadata failed"),
                    "Postgres connection succeeded but read-only migration metadata could not be queried.",
                )
                .with_subsystem("postgres")
                .with_fix_safety(FixSafety::NotFixable)
                .with_security_exposure(SecurityExposure::OperationalMetadata)
                .with_expected_actual("read-only _sqlx_migrations query succeeds", error),
                (_, Err(error)) => Check::fail(
                    "postgres_connection",
                    CheckGroup::Core,
                    "PostgreSQL",
                    format!("{summary} — migration checksum verification failed"),
                    "Postgres connection succeeded but read-only migration checksum metadata could not be queried.",
                )
                .with_subsystem("postgres")
                .with_fix_safety(FixSafety::NotFixable)
                .with_security_exposure(SecurityExposure::OperationalMetadata)
                .with_expected_actual("read-only _sqlx_migrations checksum query succeeds", error),
            }
        }
        Ok(None) => Check::ok(
            "postgres_connection",
            CheckGroup::Core,
            "PostgreSQL",
            "disabled",
        )
        .with_expected_actual("postgres bootstrap configured", "disabled"),
        Err(error) => Check::fail(
            "postgres_connection",
            CheckGroup::Core,
            "PostgreSQL",
            format!("{summary} — failed"),
            "check the DATABASE_URL or database configuration values (host/port/dbname/user/password).",
        )
        .with_expected_actual("postgres connection succeeds", error)
        .with_next_steps(vec![
            "agentdesk doctor --json".to_string(),
            format!("tail -n 200 {}", dcserver_log_hint()),
        ]),
    }
}

fn unsuccessful_migration_versions(status: &crate::db::postgres::MigrationStatus) -> Vec<i64> {
    status
        .applied
        .iter()
        .filter(|migration| !migration.success)
        .map(|migration| migration.version)
        .collect()
}

pub(super) fn checksum_mismatch_evidence(
    mismatches: &[crate::db::postgres::MigrationChecksumMismatch],
) -> Vec<Value> {
    mismatches
        .iter()
        .map(|mismatch| {
            json!({
                "version": mismatch.version,
                "applied_checksum": mismatch.applied_checksum,
                "resolved_checksum": mismatch.resolved_checksum,
            })
        })
        .collect()
}

pub(super) fn format_checksum_mismatches(
    mismatches: &[crate::db::postgres::MigrationChecksumMismatch],
) -> String {
    if mismatches.is_empty() {
        return "[]".to_string();
    }
    let parts = mismatches
        .iter()
        .map(|mismatch| {
            format!(
                "version={} applied_checksum={} resolved_checksum={}",
                mismatch.version, mismatch.applied_checksum, mismatch.resolved_checksum
            )
        })
        .collect::<Vec<_>>();
    format!("[{}]", parts.join(", "))
}

fn postgres_migration_status_is_healthy(
    status: &crate::db::postgres::MigrationStatus,
    checksum_mismatches: &[crate::db::postgres::MigrationChecksumMismatch],
) -> bool {
    status.missing_from_resolved.is_empty()
        && unsuccessful_migration_versions(status).is_empty()
        && checksum_mismatches.is_empty()
}

pub(super) fn check_stale_zero_byte_db_files(cfg: &config::Config) -> Check {
    let Some(runtime_root) = crate::config::runtime_root() else {
        return Check::warn(
            "stale_db_files",
            CheckGroup::Core,
            "Stale DB Files",
            "runtime root unresolved",
            "verify the canonical DB path and clean up zero-byte stale DB files in the runtime root.",
        )
        .with_expected_actual(
            "runtime root path resolvable",
            "runtime root path unresolved",
        );
    };

    let canonical_db_path = cfg.data.dir.join(&cfg.data.db_name);
    let stale_paths = stale_zero_byte_db_candidates(&runtime_root, &canonical_db_path);
    if crate::db::postgres::database_enabled(cfg) {
        if stale_paths.is_empty() {
            return Check::ok(
                "stale_db_files",
                CheckGroup::Core,
                "Legacy SQLite Stale Files",
                format!(
                    "no zero-byte legacy SQLite files near {}",
                    runtime_root.display()
                ),
            )
            .with_subsystem("sqlite_cache")
            .with_path(runtime_root.display().to_string())
            .with_expected_actual(
                "Postgres source-of-truth active with no stale zero-byte SQLite files",
                "no stale local artifacts found",
            );
        }

        return Check::warn(
            "stale_db_files",
            CheckGroup::Core,
            "Legacy SQLite Stale Files",
            format!(
                "{} zero-byte legacy SQLite file(s) near {}",
                stale_paths.len(),
                runtime_root.display()
            ),
            "Postgres is enabled; these zero-byte SQLite files are stale local artifacts and can be removed with the explicit repair flag.",
        )
        .with_subsystem("sqlite_cache")
        .with_fix_safety(FixSafety::ExplicitDbRepairRequired)
        .with_security_exposure(SecurityExposure::LocalPath)
        .with_path(runtime_root.display().to_string())
        .with_expected_actual(
            "Postgres source-of-truth active with no stale zero-byte SQLite files",
            format!("{} stale local artifact(s)", stale_paths.len()),
        )
        .with_next_steps(vec![
            "agentdesk doctor --fix --repair-sqlite-cache".to_string()
        ]);
    }
    if stale_paths.is_empty() {
        return Check::ok(
            "stale_db_files",
            CheckGroup::Core,
            "Stale DB Files",
            format!(
                "none near {} (canonical DB: {})",
                runtime_root.display(),
                canonical_db_path.display()
            ),
        )
        .with_path(runtime_root.display().to_string())
        .with_expected_actual("no zero-byte stale DB files", "no zero-byte stale DB files");
    }

    let listed = stale_paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    Check::warn(
        "stale_db_files",
        CheckGroup::Core,
        "Stale DB Files",
        format!("zero-byte stale DB file(s): {listed}"),
        format!(
            "the canonical DB is {}; do not open guessed paths with sqlite3. Clean up stale files with agentdesk doctor --fix if necessary.",
            canonical_db_path.display()
        ),
    )
    .with_path(runtime_root.display().to_string())
    .with_expected_actual("no zero-byte stale DB files", listed)
    .with_next_steps(vec!["agentdesk doctor --fix".to_string()])
}
