//! `agentdesk doctor` — environment diagnostics.

mod checks;
mod fix;
mod types;

use checks::{build_all_checks, fetch_health_snapshot};
use fix::{apply_safe_fixes, apply_service_fix, print_fix_actions};
use types::{
    Check, CheckGroup, CheckStatus, DoctorCheckReport, DoctorFixReport, DoctorReport,
    DoctorSummary, FixAction,
};

use crate::config;

#[cfg(test)]
use checks::{
    check_github_repo_registry, check_postgres_connection, check_provider_cli,
    check_qwen_auth_hints, check_qwen_runtime_artifacts, check_qwen_settings_files,
    check_server_running, configured_provider_names, discord_bot_check_from_health,
    provider_capability_summary,
};
#[cfg(test)]
use types::HealthSnapshot;

fn summarize_checks(checks: &[Check]) -> DoctorSummary {
    let mut passed = 0;
    let mut warned = 0;
    let mut failed = 0;

    for check in checks {
        match check.status {
            CheckStatus::Pass => passed += 1,
            CheckStatus::Warn => warned += 1,
            CheckStatus::Fail => failed += 1,
        }
    }

    DoctorSummary {
        passed,
        warned,
        failed,
        total: checks.len(),
    }
}

fn build_json_report(fix_requested: bool, checks: &[Check], actions: &[FixAction]) -> DoctorReport {
    let summary = summarize_checks(checks);
    let checks = checks
        .iter()
        .map(|check| DoctorCheckReport {
            id: check.id,
            group: check.group.as_str(),
            name: check.name,
            status: check.status.as_str(),
            ok: matches!(check.status, CheckStatus::Pass),
            detail: check.detail.clone(),
            guidance: check.guidance.clone(),
            path: check.path.clone(),
            expected: check.expected.clone(),
            actual: check.actual.clone(),
            next_steps: check.next_steps.clone(),
        })
        .collect();
    let fixes = actions
        .iter()
        .map(|action| DoctorFixReport {
            id: action.id,
            name: action.name,
            status: if action.ok { "applied" } else { "failed" },
            ok: action.ok,
            detail: action.detail.clone(),
        })
        .collect();

    DoctorReport {
        version: env!("CARGO_PKG_VERSION"),
        ok: summary.failed == 0,
        fix_requested,
        summary,
        checks,
        fixes,
    }
}

fn print_group(title: &str, checks: &[Check]) {
    if checks.is_empty() {
        return;
    }
    println!("{title}");
    for check in checks {
        println!(
            "  {} [{}] {}: {}",
            check.icon(),
            check.label(),
            check.name,
            check.detail
        );
        if let Some(guidance) = &check.guidance {
            if !guidance.trim().is_empty() {
                println!("      → {}", guidance);
            }
        }
    }
    println!();
}

pub fn cmd_doctor(fix: bool, json: bool) -> Result<(), String> {
    let cfg = config::load_graceful();
    let mut actions = Vec::new();
    if fix {
        actions = apply_safe_fixes(&cfg);
        let pre_fix_snapshot = fetch_health_snapshot();
        actions.extend(apply_service_fix(&pre_fix_snapshot));
        if !json {
            println!("AgentDesk Doctor v{}\n", env!("CARGO_PKG_VERSION"));
            print_fix_actions(&actions);
        }
    } else if !json {
        println!("AgentDesk Doctor v{}\n", env!("CARGO_PKG_VERSION"));
    }

    let snapshot = fetch_health_snapshot();
    let checks = build_all_checks(&cfg, &snapshot);
    let summary = summarize_checks(&checks);

    if json {
        let report = build_json_report(fix, &checks, &actions);
        println!(
            "{}",
            serde_json::to_string_pretty(&report)
                .map_err(|e| format!("failed to serialize doctor report: {e}"))?
        );
    } else {
        let core_checks: Vec<Check> = checks
            .iter()
            .filter(|check| matches!(check.group, CheckGroup::Core))
            .cloned()
            .collect();
        let provider_checks: Vec<Check> = checks
            .iter()
            .filter(|check| matches!(check.group, CheckGroup::ProviderRuntime))
            .cloned()
            .collect();

        print_group("Core", &core_checks);
        print_group("Provider Runtime", &provider_checks);

        println!(
            "  {} passed, {} warned, {} failed out of {} checks",
            summary.passed, summary.warned, summary.failed, summary.total
        );
    }

    if summary.failed > 0 {
        Err(format!("{} diagnostic check(s) failed", summary.failed))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Check, CheckGroup, CheckStatus, FixAction, HealthSnapshot, build_json_report,
        check_github_repo_registry, check_postgres_connection, check_provider_cli,
        check_qwen_auth_hints, check_qwen_runtime_artifacts, check_qwen_settings_files,
        check_server_running, configured_provider_names, discord_bot_check_from_health,
        provider_capability_summary,
    };
    use crate::config::ServerConfig;
    use crate::db::schema;
    use crate::services::provider::ProviderKind;
    use serde_json::json;
    use std::path::Path;

    fn with_temp_qwen_doctor_env<F>(f: F)
    where
        F: FnOnce(&tempfile::TempDir, &tempfile::TempDir),
    {
        let _guard = crate::services::discord::runtime_store::lock_test_env();
        let temp_home = tempfile::tempdir().unwrap();
        let temp_project = tempfile::tempdir().unwrap();
        let prev_qwen_home = std::env::var_os("QWEN_HOME");
        let prev_home = std::env::var_os("HOME");
        let prev_userprofile = std::env::var_os("USERPROFILE");
        let prev_test_home = std::env::var_os("AGENTDESK_TEST_HOME");
        let prev_cwd = std::env::current_dir().unwrap();

        unsafe {
            std::env::set_var("HOME", temp_home.path());
            std::env::set_var("USERPROFILE", temp_home.path());
            std::env::set_var("AGENTDESK_TEST_HOME", temp_home.path());
        }
        std::env::set_current_dir(temp_project.path()).unwrap();

        f(&temp_home, &temp_project);

        std::env::set_current_dir(prev_cwd).unwrap();
        match prev_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        match prev_qwen_home {
            Some(value) => unsafe { std::env::set_var("QWEN_HOME", value) },
            None => unsafe { std::env::remove_var("QWEN_HOME") },
        }
        match prev_userprofile {
            Some(value) => unsafe { std::env::set_var("USERPROFILE", value) },
            None => unsafe { std::env::remove_var("USERPROFILE") },
        }
        match prev_test_home {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_TEST_HOME", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_TEST_HOME") },
        }
    }

    #[cfg(unix)]
    fn write_executable(path: &Path, contents: &str) {
        use std::os::unix::fs::PermissionsExt;

        std::fs::write(path, contents).unwrap();
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).unwrap();
    }

    fn write_github_repo_registry(db_path: &Path, repos: &[&str]) {
        let conn = libsql_rusqlite::Connection::open(db_path).unwrap();
        schema::migrate(&conn).unwrap();
        for repo in repos {
            conn.execute(
                "INSERT INTO github_repos (id, display_name, sync_enabled) VALUES (?1, ?1, 1)",
                [repo],
            )
            .unwrap();
        }
    }

    fn test_base_url() -> String {
        format!(
            "http://{}:{}",
            ServerConfig::loopback(),
            ServerConfig::default().port,
        )
    }

    #[test]
    fn unified_health_requires_connected_providers() {
        let check = discord_bot_check_from_health(
            &test_base_url(),
            &json!({
                "status": "healthy",
                "providers": [
                    {"name": "claude", "connected": true},
                    {"name": "codex", "connected": true}
                ]
            }),
        );
        assert_eq!(check.status, CheckStatus::Pass);
        assert!(check.detail.contains("2/2 connected"));
    }

    #[test]
    fn standalone_health_becomes_warning() {
        let check = discord_bot_check_from_health(
            &test_base_url(),
            &json!({
                "ok": true,
                "db": true,
                "version": "0.1.0"
            }),
        );
        assert_eq!(check.status, CheckStatus::Warn);
        assert!(check.detail.contains("standalone health only"));
    }

    #[test]
    fn degraded_health_fails_server_check() {
        let snapshot = HealthSnapshot {
            base: test_base_url(),
            body: Some(json!({
                "status": "degraded",
                "version": "0.1.0"
            })),
            error: None,
        };

        let check = check_server_running(&snapshot);
        assert_eq!(check.status, CheckStatus::Fail);
        assert!(check.detail.contains("degraded"));
    }

    #[test]
    fn provider_capability_summary_mentions_structured_contract() {
        let summary = provider_capability_summary(&ProviderKind::Codex);
        assert!(summary.contains("structured-output"));
        assert!(summary.contains("resume"));
        assert!(summary.contains("tool-stream"));
    }

    #[test]
    fn configured_provider_names_merges_config_and_health() {
        let mut cfg = crate::config::Config::default();
        cfg.agents.push(crate::config::AgentDef {
            id: "agent-1".to_string(),
            name: "Agent 1".to_string(),
            name_ko: None,
            provider: "codex".to_string(),
            channels: crate::config::AgentChannels::default(),
            keywords: Vec::new(),
            department: None,
            avatar_emoji: None,
        });

        let snapshot = HealthSnapshot {
            base: test_base_url(),
            body: Some(json!({
                "status": "healthy",
                "providers": [
                    {"name": "claude", "connected": true}
                ]
            })),
            error: None,
        };

        let configured = configured_provider_names(&cfg, &snapshot);
        assert!(configured.contains("claude"));
        assert!(configured.contains("codex"));
    }

    #[cfg(unix)]
    #[test]
    fn provider_runtime_check_uses_resolver_exec_path_under_minimal_path() {
        let _guard = crate::services::discord::runtime_store::lock_test_env();
        let temp = tempfile::tempdir().unwrap();
        let helper = temp.path().join("provider-helper");
        let provider = temp.path().join("codex");
        let original_path = std::env::var_os("PATH");

        write_executable(&helper, "#!/bin/sh\nprintf 'codex-test 1.2.3\\n'\n");
        write_executable(
            &provider,
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then\n  provider-helper\nelse\n  exit 64\nfi\n",
        );

        unsafe {
            std::env::set_var("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
            std::env::set_var("AGENTDESK_CODEX_PATH", &provider);
        }

        let snapshot = HealthSnapshot {
            base: test_base_url(),
            body: None,
            error: None,
        };
        let check = check_provider_cli(ProviderKind::Codex, true, &snapshot);

        assert_eq!(check.status, CheckStatus::Pass);
        assert!(check.detail.contains("codex-test 1.2.3"));
        assert!(check.detail.contains("env_override"));
        assert_eq!(
            check.path.as_deref(),
            Some(provider.to_string_lossy().as_ref())
        );

        unsafe {
            std::env::remove_var("AGENTDESK_CODEX_PATH");
            match original_path {
                Some(value) => std::env::set_var("PATH", value),
                None => std::env::remove_var("PATH"),
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn provider_runtime_check_reports_permission_denied() {
        use std::os::unix::fs::PermissionsExt;

        let _guard = crate::services::discord::runtime_store::lock_test_env();
        let temp = tempfile::tempdir().unwrap();
        let provider = temp.path().join("codex");
        let original_path = std::env::var_os("PATH");

        std::fs::write(&provider, "#!/bin/sh\nprintf 'codex-test 1.2.3\\n'\n").unwrap();
        let mut perms = std::fs::metadata(&provider).unwrap().permissions();
        perms.set_mode(0o644);
        std::fs::set_permissions(&provider, perms).unwrap();

        unsafe {
            std::env::set_var("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
            std::env::set_var("AGENTDESK_CODEX_PATH", &provider);
        }

        let snapshot = HealthSnapshot {
            base: test_base_url(),
            body: None,
            error: None,
        };
        let check = check_provider_cli(ProviderKind::Codex, true, &snapshot);

        assert_eq!(check.status, CheckStatus::Fail);
        assert!(check.detail.contains("permission_denied"));
        assert!(check.detail.contains("not found in runtime PATH"));
        assert_eq!(check.path.as_deref(), None);

        unsafe {
            std::env::remove_var("AGENTDESK_CODEX_PATH");
            match original_path {
                Some(value) => std::env::set_var("PATH", value),
                None => std::env::remove_var("PATH"),
            }
        }
    }

    #[test]
    fn qwen_doctor_detects_home_extensions_not_project_extensions() {
        with_temp_qwen_doctor_env(|temp_home, temp_project| {
            let home_qwen = temp_home.path().join(".qwen");
            let project_qwen = temp_project.path().join(".qwen");
            std::fs::create_dir_all(home_qwen.join("extensions")).unwrap();
            std::fs::create_dir_all(&project_qwen).unwrap();

            let check = check_qwen_runtime_artifacts(true);
            assert_eq!(check.status, CheckStatus::Pass);
            assert!(check.detail.contains("home: extensions"));
            assert!(check.detail.contains("project: -"));
        });
    }

    #[test]
    fn qwen_doctor_surfaces_project_summary_and_output_language() {
        with_temp_qwen_doctor_env(|temp_home, temp_project| {
            let home_qwen = temp_home.path().join(".qwen");
            let project_qwen = temp_project.path().join(".qwen");
            std::fs::create_dir_all(&home_qwen).unwrap();
            std::fs::create_dir_all(&project_qwen).unwrap();
            std::fs::write(home_qwen.join("output-language.md"), "Korean").unwrap();
            std::fs::write(project_qwen.join("PROJECT_SUMMARY.md"), "Summary").unwrap();

            let check = check_qwen_runtime_artifacts(true);
            assert_eq!(check.status, CheckStatus::Pass);
            assert!(check.detail.contains("output-language.md"));
            assert!(check.detail.contains("PROJECT_SUMMARY.md"));
        });
    }

    #[test]
    fn qwen_doctor_auth_hint_prefers_project_dot_qwen_env_before_dot_env() {
        with_temp_qwen_doctor_env(|_temp_home, temp_project| {
            let project_qwen = temp_project.path().join(".qwen");
            std::fs::create_dir_all(&project_qwen).unwrap();
            std::fs::write(project_qwen.join(".env"), "DASHSCOPE_API_KEY=one").unwrap();
            std::fs::write(temp_project.path().join(".env"), "OPENAI_API_KEY=two").unwrap();

            let check = check_qwen_auth_hints(true);
            assert_eq!(check.status, CheckStatus::Pass);
            assert!(check.detail.contains("project .qwen/.env"));
            assert!(check.detail.contains("project .env"));
        });
    }

    #[test]
    fn qwen_doctor_prefers_qwen_home_over_home() {
        with_temp_qwen_doctor_env(|temp_home, _temp_project| {
            let qwen_home = tempfile::tempdir().unwrap();
            let qwen_settings = qwen_home.path().join(".qwen").join("settings.json");
            std::fs::create_dir_all(qwen_settings.parent().unwrap()).unwrap();
            std::fs::write(&qwen_settings, "{}").unwrap();

            unsafe {
                std::env::set_var("QWEN_HOME", qwen_home.path());
                std::env::set_var("HOME", temp_home.path());
            }

            let check = check_qwen_settings_files(true);
            assert_eq!(check.status, CheckStatus::Pass);
            assert_eq!(
                check.path.as_deref(),
                Some(format!("user settings={}", qwen_settings.display()).as_str())
            );
        });
    }

    #[test]
    fn github_repo_registry_check_passes_when_config_matches_db() {
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("agentdesk.db");
        write_github_repo_registry(&db_path, &["owner/repo-a", "owner/repo-b"]);

        let mut cfg = crate::config::Config::default();
        cfg.data.dir = temp.path().to_path_buf();
        cfg.data.db_name = "agentdesk.db".to_string();
        cfg.github.repos = vec![
            " owner/repo-b ".to_string(),
            "owner/repo-a".to_string(),
            "owner/repo-a".to_string(),
        ];

        let check = check_github_repo_registry(&cfg);
        assert_eq!(check.status, CheckStatus::Pass);
        assert!(check.detail.contains("config=2 db=2"));
    }

    #[test]
    fn github_repo_registry_check_reports_missing_extra_and_invalid_entries() {
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("agentdesk.db");
        write_github_repo_registry(&db_path, &["owner/repo-a", "owner/repo-stale"]);

        let mut cfg = crate::config::Config::default();
        cfg.data.dir = temp.path().to_path_buf();
        cfg.data.db_name = "agentdesk.db".to_string();
        cfg.github.repos = vec![
            "owner/repo-a".to_string(),
            "owner/repo-missing".to_string(),
            "noslash".to_string(),
        ];

        let check = check_github_repo_registry(&cfg);
        assert_eq!(check.status, CheckStatus::Warn);
        assert!(check.detail.contains("missing_in_db=owner/repo-missing"));
        assert!(check.detail.contains("extra_in_db=owner/repo-stale"));
        assert!(check.detail.contains("invalid_config=noslash"));
    }

    #[test]
    fn postgres_connection_check_is_ok_when_disabled() {
        let config = crate::config::Config::default();
        let check = check_postgres_connection(&config);
        assert_eq!(check.status, CheckStatus::Pass);
        assert_eq!(check.detail, "disabled");
    }

    #[test]
    fn json_report_uses_stable_machine_friendly_fields() {
        let checks = vec![
            Check::warn(
                "service_manager",
                CheckGroup::Core,
                "Service Manager",
                "systemd inactive",
                "restart service",
            )
            .with_path("systemctl --user")
            .with_expected_actual("service active", "service inactive")
            .with_next_steps(vec![
                "systemctl --user status agentdesk-dcserver".to_string(),
                "agentdesk doctor --fix".to_string(),
            ]),
        ];
        let fixes = vec![FixAction::ok(
            "service_restart",
            "Service Restart",
            "systemd restart succeeded",
        )];

        let report = build_json_report(true, &checks, &fixes);
        let value = serde_json::to_value(report).expect("serialize doctor report");

        assert_eq!(value["fix_requested"], true);
        assert_eq!(value["summary"]["warned"], 1);
        assert_eq!(value["checks"][0]["id"], "service_manager");
        assert_eq!(value["checks"][0]["group"], "core");
        assert_eq!(value["checks"][0]["status"], "warn");
        assert_eq!(value["checks"][0]["path"], "systemctl --user");
        assert_eq!(value["checks"][0]["expected"], "service active");
        assert_eq!(value["checks"][0]["actual"], "service inactive");
        assert_eq!(
            value["checks"][0]["next_steps"][0],
            "systemctl --user status agentdesk-dcserver"
        );
        assert_eq!(value["fixes"][0]["id"], "service_restart");
        assert_eq!(value["fixes"][0]["status"], "applied");
    }
}
