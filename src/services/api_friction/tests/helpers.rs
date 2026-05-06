use sqlx::PgPool;
use std::fs;

pub(super) struct MockGhIssueCreateEnv {
    _dir: tempfile::TempDir,
    old_gh_path: Option<std::ffi::OsString>,
}

pub(super) struct TestPostgresDb {
    _lock: crate::db::postgres::PostgresTestLifecycleGuard,
    admin_url: String,
    database_name: String,
    database_url: String,
}

impl TestPostgresDb {
    pub(super) async fn create() -> Self {
        let lock = crate::db::postgres::lock_test_lifecycle();
        let admin_url = postgres_admin_database_url();
        let database_name = format!("agentdesk_api_friction_{}", uuid::Uuid::new_v4().simple());
        let database_url = format!("{}/{}", postgres_base_database_url(), database_name);
        crate::db::postgres::create_test_database(&admin_url, &database_name, "api_friction tests")
            .await
            .unwrap();

        Self {
            _lock: lock,
            admin_url,
            database_name,
            database_url,
        }
    }

    pub(super) async fn connect_and_migrate(&self) -> PgPool {
        crate::db::postgres::connect_test_pool_and_migrate(&self.database_url, "api_friction tests")
            .await
            .unwrap()
    }

    pub(super) async fn drop(self) {
        crate::db::postgres::drop_test_database(
            &self.admin_url,
            &self.database_name,
            "api_friction tests",
        )
        .await
        .unwrap();
    }
}

fn postgres_base_database_url() -> String {
    if let Ok(base) = std::env::var("POSTGRES_TEST_DATABASE_URL_BASE") {
        let trimmed = base.trim();
        if !trimmed.is_empty() {
            return trimmed.trim_end_matches('/').to_string();
        }
    }

    let user = std::env::var("PGUSER")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            std::env::var("USER")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .unwrap_or_else(|| "postgres".to_string());
    let password = std::env::var("PGPASSWORD")
        .ok()
        .filter(|value| !value.trim().is_empty());
    let host = std::env::var("PGHOST")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "localhost".to_string());
    let port = std::env::var("PGPORT")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "5432".to_string());

    match password {
        Some(password) => format!("postgresql://{user}:{password}@{host}:{port}"),
        None => format!("postgresql://{user}@{host}:{port}"),
    }
}

fn postgres_admin_database_url() -> String {
    let admin_db = std::env::var("POSTGRES_TEST_ADMIN_DB")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "postgres".to_string());
    format!("{}/{}", postgres_base_database_url(), admin_db)
}

impl Drop for MockGhIssueCreateEnv {
    fn drop(&mut self) {
        if let Some(value) = &self.old_gh_path {
            unsafe { std::env::set_var("AGENTDESK_GH_PATH", value) };
        } else {
            unsafe { std::env::remove_var("AGENTDESK_GH_PATH") };
        }
    }
}

#[cfg(unix)]
pub(super) fn install_mock_gh_issue_create(url: &str) -> MockGhIssueCreateEnv {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let gh_path = dir.path().join("gh");
    let script = format!(
        "#!/bin/sh\nset -eu\nif [ \"${{1-}}\" = \"issue\" ] && [ \"${{2-}}\" = \"create\" ]; then\ncat <<'EOF'\n{url}\nEOF\nexit 0\nfi\nexit 1\n"
    );
    fs::write(&gh_path, script).unwrap();
    let mut perms = fs::metadata(&gh_path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&gh_path, perms).unwrap();

    let old_gh_path = std::env::var_os("AGENTDESK_GH_PATH");
    unsafe { std::env::set_var("AGENTDESK_GH_PATH", &gh_path) };

    MockGhIssueCreateEnv {
        _dir: dir,
        old_gh_path,
    }
}

#[cfg(windows)]
pub(super) fn install_mock_gh_issue_create(url: &str) -> MockGhIssueCreateEnv {
    let dir = tempfile::tempdir().unwrap();
    let gh_cmd_path = dir.path().join("gh.cmd");
    let wrapper = format!(
        "@echo off\r\nsetlocal\r\nif /I \"%~1\"==\"--version\" goto version\r\nif /I not \"%~1\"==\"issue\" exit /b 1\r\nif /I not \"%~2\"==\"create\" exit /b 1\r\necho {url}\r\nexit /b 0\r\n:version\r\necho gh mock 1.0\r\nexit /b 0\r\n"
    );
    fs::write(&gh_cmd_path, wrapper).unwrap();

    let old_gh_path = std::env::var_os("AGENTDESK_GH_PATH");
    unsafe { std::env::set_var("AGENTDESK_GH_PATH", &gh_cmd_path) };

    MockGhIssueCreateEnv {
        _dir: dir,
        old_gh_path,
    }
}

pub(super) fn restore_env(name: &str, previous: Option<std::ffi::OsString>) {
    match previous {
        Some(value) => unsafe { std::env::set_var(name, value) },
        None => unsafe { std::env::remove_var(name) },
    }
}
