//! Doctor checks for the configured directories: `data.dir`, `policies.dir`
//! and the routine script directories (`routines.dir` + `additional_dirs`)
//! versus the `routines` table (#5727).

use std::path::PathBuf;

use super::{Check, CheckGroup};
use crate::config;
use serde_json::json;

const ROUTINE_SCRIPT_REGISTRATION_CHECK_ID: &str = "routine_scripts_registered";
const ROUTINE_SCRIPT_REGISTRATION_CHECK_NAME: &str = "Routine Scripts";

/// Pure builder for the routine-script registration finding so the message
/// contract ("폴더에 있으나 미등록 N개: …") is unit-testable without a DB.
fn routine_script_registration_check(
    dirs: &[PathBuf],
    discovered_count: usize,
    unregistered: &[String],
) -> Check {
    let dirs_display = dirs
        .iter()
        .map(|dir| dir.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let registered_count = discovered_count.saturating_sub(unregistered.len());
    if unregistered.is_empty() {
        return Check::ok(
            ROUTINE_SCRIPT_REGISTRATION_CHECK_ID,
            CheckGroup::Core,
            ROUTINE_SCRIPT_REGISTRATION_CHECK_NAME,
            format!("{dirs_display} — 스크립트 {discovered_count}개 모두 routines 테이블에 등록됨"),
        )
        .with_subsystem("routines")
        .with_path(dirs_display)
        .with_expected_actual(
            "every *.js under routines.dir has a routines row",
            format!("discovered={discovered_count} unregistered=0"),
        )
        .with_evidence(json!({
            "discovered": discovered_count,
            "registered": registered_count,
            "unregistered": [],
        }));
    }
    Check::warn(
        ROUTINE_SCRIPT_REGISTRATION_CHECK_ID,
        CheckGroup::Core,
        ROUTINE_SCRIPT_REGISTRATION_CHECK_NAME,
        format!(
            "폴더에 있으나 미등록 {}개: {}",
            unregistered.len(),
            unregistered.join(", ")
        ),
        "routines.dir 의 *.js 가 routines 테이블에 attach 되지 않았습니다. POST /api/routines 로 등록하거나 파일을 제거하세요.",
    )
    .with_subsystem("routines")
    .with_path(dirs_display)
    .with_expected_actual(
        "every *.js under routines.dir has a routines row",
        format!(
            "discovered={discovered_count} unregistered={}",
            unregistered.len()
        ),
    )
    .with_evidence(json!({
        "discovered": discovered_count,
        "registered": registered_count,
        "unregistered": unregistered,
    }))
    .with_next_steps(vec![
        "curl -X POST http://127.0.0.1:<port>/api/routines -d '{\"script_ref\":\"<file.js>\", ...}'"
            .to_string(),
    ])
}

pub(super) fn check_routine_script_registration(cfg: &config::Config) -> Check {
    use crate::services::routines::{
        discover_routine_script_refs, registered_routine_script_refs,
        unregistered_routine_script_refs,
    };

    let dirs = cfg.routines.script_dirs();
    let dirs_display = dirs
        .iter()
        .map(|dir| dir.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    if !cfg.routines.enabled {
        return Check::ok(
            ROUTINE_SCRIPT_REGISTRATION_CHECK_ID,
            CheckGroup::Core,
            ROUTINE_SCRIPT_REGISTRATION_CHECK_NAME,
            "routines disabled",
        )
        .with_subsystem("routines")
        .with_path(dirs_display)
        .with_expected_actual(
            "routine registration check applies",
            "routines.enabled=false",
        );
    }
    if !crate::db::postgres::database_enabled(cfg) {
        return Check::ok(
            ROUTINE_SCRIPT_REGISTRATION_CHECK_ID,
            CheckGroup::Core,
            ROUTINE_SCRIPT_REGISTRATION_CHECK_NAME,
            "postgres disabled — registration check skipped",
        )
        .with_subsystem("routines")
        .with_path(dirs_display)
        .with_expected_actual("routine registration check applies", "postgres disabled");
    }

    let discovered = discover_routine_script_refs(&dirs);
    if discovered.is_empty() {
        return routine_script_registration_check(&dirs, 0, &[]);
    }

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            return Check::warn(
                ROUTINE_SCRIPT_REGISTRATION_CHECK_ID,
                CheckGroup::Core,
                ROUTINE_SCRIPT_REGISTRATION_CHECK_NAME,
                format!("{dirs_display} — runtime init failed"),
                "routines 테이블 조회용 async runtime 생성에 실패했습니다.",
            )
            .with_subsystem("routines")
            .with_path(dirs_display)
            .with_expected_actual(
                "routine registration check runtime initializes",
                format!("runtime build failed: {error}"),
            );
        }
    };
    let registered = runtime.block_on(async {
        match crate::db::postgres::connect(cfg).await {
            Ok(Some(pool)) => {
                let registered = registered_routine_script_refs(&pool).await;
                drop(pool);
                registered.map_err(|error| error.to_string())
            }
            Ok(None) => Err("postgres pool unavailable".to_string()),
            Err(error) => Err(error.to_string()),
        }
    });
    match registered {
        Ok(registered) => {
            let unregistered = unregistered_routine_script_refs(&discovered, &registered);
            routine_script_registration_check(&dirs, discovered.len(), &unregistered)
        }
        Err(error) => Check::warn(
            ROUTINE_SCRIPT_REGISTRATION_CHECK_ID,
            CheckGroup::Core,
            ROUTINE_SCRIPT_REGISTRATION_CHECK_NAME,
            format!("{dirs_display} — routines 테이블 조회 실패"),
            "Postgres 연결 또는 routines 테이블 조회에 실패해 미등록 스크립트를 판정하지 못했습니다.",
        )
        .with_subsystem("routines")
        .with_path(dirs_display)
        .with_expected_actual("SELECT script_ref FROM routines succeeds", error),
    }
}

pub(super) fn check_policies_dir(cfg: &config::Config) -> Check {
    if cfg.policies.dir.exists() && cfg.policies.dir.is_dir() {
        Check::ok(
            "policies_directory",
            CheckGroup::Core,
            "Policies Directory",
            format!("{}", cfg.policies.dir.display()),
        )
        .with_path(cfg.policies.dir.display().to_string())
        .with_expected_actual("policies directory exists", "policies directory exists")
    } else {
        Check::fail(
            "policies_directory",
            CheckGroup::Core,
            "Policies Directory",
            format!("{} — missing", cfg.policies.dir.display()),
            "Create a policies folder at this path or correct the policies.dir path in agentdesk.yaml.",
        )
        .with_path(cfg.policies.dir.display().to_string())
        .with_expected_actual("policies directory exists", "policies directory missing")
    }
}

pub(super) fn check_data_dir(cfg: &config::Config) -> Check {
    if cfg.data.dir.exists() && cfg.data.dir.is_dir() {
        Check::ok(
            "data_directory",
            CheckGroup::Core,
            "Data Directory",
            format!("{}", cfg.data.dir.display()),
        )
        .with_path(cfg.data.dir.display().to_string())
        .with_expected_actual("data directory exists", "data directory exists")
    } else {
        Check::fail(
            "data_directory",
            CheckGroup::Core,
            "Data Directory",
            format!("{} — missing", cfg.data.dir.display()),
            "agentdesk doctor --fix 로 data 디렉터리와 DB를 생성할 수 있습니다.",
        )
        .with_path(cfg.data.dir.display().to_string())
        .with_expected_actual("data directory exists", "data directory missing")
        .with_next_steps(vec!["agentdesk doctor --fix".to_string()])
    }
}

#[cfg(test)]
mod routine_script_registration_tests {
    use super::super::CheckStatus;
    use super::{
        CheckGroup, ROUTINE_SCRIPT_REGISTRATION_CHECK_ID, routine_script_registration_check,
    };
    use std::path::PathBuf;

    #[test]
    fn unregistered_scripts_are_reported_as_one_warn_item() {
        let dirs = vec![PathBuf::from("/opt/adk/routines")];
        let unregistered = vec![
            "dependency-update-watcher.js".to_string(),
            "nested/x.js".to_string(),
        ];

        let check = routine_script_registration_check(&dirs, 5, &unregistered);

        assert_eq!(check.id, ROUTINE_SCRIPT_REGISTRATION_CHECK_ID);
        assert_eq!(check.group, CheckGroup::Core);
        assert_eq!(check.status, CheckStatus::Warn);
        assert_eq!(check.subsystem, "routines");
        assert_eq!(
            check.detail,
            "폴더에 있으나 미등록 2개: dependency-update-watcher.js, nested/x.js"
        );
        assert_eq!(check.path.as_deref(), Some("/opt/adk/routines"));
        assert_eq!(check.actual.as_deref(), Some("discovered=5 unregistered=2"));
        let evidence = check.evidence.expect("evidence");
        assert_eq!(evidence["discovered"], 5);
        assert_eq!(evidence["registered"], 3);
        assert_eq!(
            evidence["unregistered"],
            serde_json::json!(["dependency-update-watcher.js", "nested/x.js"])
        );
        assert!(
            check
                .guidance
                .as_deref()
                .unwrap_or("")
                .contains("POST /api/routines")
        );
        assert!(!check.next_steps.is_empty());
    }

    #[test]
    fn fully_registered_scripts_pass() {
        let dirs = vec![
            PathBuf::from("/opt/adk/routines"),
            PathBuf::from("/opt/adk/routines-extra"),
        ];
        let check = routine_script_registration_check(&dirs, 3, &[]);

        assert_eq!(check.status, CheckStatus::Pass);
        assert!(check.detail.contains("3개 모두"));
        assert_eq!(
            check.path.as_deref(),
            Some("/opt/adk/routines, /opt/adk/routines-extra")
        );
        assert_eq!(check.actual.as_deref(), Some("discovered=3 unregistered=0"));
        assert_eq!(
            check.evidence.expect("evidence")["unregistered"],
            serde_json::json!([])
        );
    }

    #[test]
    fn empty_directory_passes_with_zero_counts() {
        let check = routine_script_registration_check(&[PathBuf::from("/none")], 0, &[]);
        assert_eq!(check.status, CheckStatus::Pass);
        assert_eq!(check.evidence.expect("evidence")["discovered"], 0);
    }
}
