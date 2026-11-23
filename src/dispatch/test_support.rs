//! Re-exports the KRX PostgreSQL test fixture under its historical path for upper tests.

pub(crate) use crate::db::dispatch_test_db::{
    DispatchPostgresTestDb, postgres_admin_database_url, postgres_base_database_url,
    seed_pg_dispatch,
};

#[cfg(test)]
mod tests {
    use super::{DispatchPostgresTestDb, postgres_base_database_url};
    use crate::test_support::wait::{WAIT_CAP, await_within, join_within};

    #[test]
    fn try_create_pg_contract() {
        const PROBE: &str = "AGENTDESK_DISPATCH_FIXTURE_PROBE";
        if std::env::var_os(PROBE).is_some() {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(await_within(
                async {
                    let _fixture = DispatchPostgresTestDb::try_create(
                        "agentdesk_probe",
                        "dispatch fixture probe",
                    )
                    .await;
                },
                WAIT_CAP,
            ));
            return;
        }
        let _lifecycle = crate::db::postgres::lock_test_lifecycle();
        let base = postgres_base_database_url();
        for required in [None, Some("0"), Some("1")] {
            for configured in [false, true] {
                let admin_db = format!("agentdesk_missing_{}", uuid::Uuid::new_v4().simple());
                let mut command = std::process::Command::new(std::env::current_exe().unwrap());
                command
                    .args([
                        "--exact",
                        "dispatch::test_support::tests::try_create_pg_contract",
                        "--nocapture",
                    ])
                    .env(PROBE, "1")
                    .env("AGENTDESK_TEST_POSTGRES_ACQUIRE_TIMEOUT_MS", "250")
                    .env("POSTGRES_TEST_ADMIN_DB", &admin_db);
                if let Some(required) = required {
                    command.env("AGENTDESK_REQUIRE_PG", required);
                } else {
                    command.env_remove("AGENTDESK_REQUIRE_PG");
                }
                if configured {
                    command.env("POSTGRES_TEST_DATABASE_URL_BASE", &base);
                } else {
                    command.env_remove("POSTGRES_TEST_DATABASE_URL_BASE");
                }
                let output = join_within(
                    std::thread::spawn(move || command.output().unwrap()),
                    WAIT_CAP,
                );
                let stderr = String::from_utf8_lossy(&output.stderr);
                let stdout = String::from_utf8_lossy(&output.stdout);
                assert!(
                    !output.status.success(),
                    "required={required:?} configured={configured}: {stdout} {stderr}"
                );
                assert!(
                    stdout.contains("running 1 test")
                        && stdout.contains("test result: FAILED. 0 passed; 1 failed; 0 ignored;"),
                    "required={required:?} configured={configured}: {stdout} {stderr}"
                );
                if configured {
                    assert!(stderr.contains("dispatch fixture probe admin"), "{stderr}");
                    assert!(stderr.contains(&admin_db), "{stderr}");
                } else {
                    assert!(
                        stderr.contains(if required == Some("1") {
                            "PG required"
                        } else {
                            "POSTGRES_TEST_DATABASE_URL_BASE required"
                        }),
                        "{stderr}"
                    );
                }
                eprintln!(
                    "fixture contract required={required:?} configured={configured} child_success={}",
                    output.status.success()
                );
            }
        }
    }
}
