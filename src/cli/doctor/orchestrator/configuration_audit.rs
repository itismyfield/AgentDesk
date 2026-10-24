//! Read-only checks over the runtime prompt mirror and managed skill manifests.
use super::{Check, CheckGroup};
use crate::server::routes::skills_manifest_audit as skills;
use std::path::Path;

pub(super) fn checks(cfg: &crate::config::Config) -> Vec<Check> {
    let prompt = match crate::config::runtime_root() {
        Some(root) => prompt_api_routes_check(
            &root.join("workspaces").join("agentdesk"),
            &crate::runtime_layout::managed_agents_root(&root),
        ),
        None => prompt_result(
            None,
            "inspection_unavailable: runtime root unresolved".into(),
        ),
    };
    vec![prompt, skill_manifest_check(&doctor_manifest_report(cfg))]
}

fn prompt_api_routes_check(repo: &Path, prompts: &Path) -> Check {
    // Invoke deploy staging's checker from the same checkout as its inventory.
    let output = std::process::Command::new("python3")
        .arg(repo.join("scripts/check_prompt_api_routes.py"))
        .arg("--repo")
        .arg(repo)
        .arg("--prompts")
        .arg(prompts)
        .output();
    let (code, detail) = match output {
        Ok(output) => (
            output.status.code(),
            String::from_utf8_lossy(&output.stdout).trim().to_string(),
        ),
        Err(error) => (None, format!("inspection_unavailable: {error}")),
    };
    prompt_result(code, detail).with_path(prompts.display().to_string())
}

fn prompt_result(code: Option<i32>, output: String) -> Check {
    let detail = if matches!(code, Some(0 | 1)) {
        output
    } else {
        format!("확인 불가 / inspection_unavailable: {output}")
    };
    let check = if code == Some(0) {
        Check::ok(
            "prompt_api_routes",
            CheckGroup::Core,
            "Prompt API Routes",
            detail,
        )
    } else {
        Check::warn(
            "prompt_api_routes",
            CheckGroup::Core,
            "Prompt API Routes",
            detail,
            "Inspect the runtime config/agents mirror and workspaces/agentdesk endpoint inventory; doctor only diagnoses.",
        )
    };
    check.with_subsystem("config_audit")
}

fn doctor_manifest_report(cfg: &crate::config::Config) -> skills::ManifestAuditReport {
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return skills::audit_skill_manifest_agents(skills::unavailable_manifest_audit_request(
            cfg,
        ));
    };
    runtime.block_on(async {
        // An isolated one-connection pool performs only the roster SELECT, without migrations.
        let pool = match crate::db::postgres::connect_options(cfg) {
            Ok(Some(options)) => tokio::time::timeout(
                std::time::Duration::from_secs(2),
                sqlx::postgres::PgPoolOptions::new()
                    .max_connections(1)
                    .connect_with(options),
            )
            .await
            .ok()
            .and_then(Result::ok),
            _ => None,
        };
        let request = match pool.as_ref() {
            Some(pool) => skills::manifest_audit_request(cfg, pool).await,
            None => skills::unavailable_manifest_audit_request(cfg),
        };
        let report = skills::audit_skill_manifest_agents(request);
        if let Some(pool) = pool {
            pool.close().await;
        }
        report
    })
}

fn skill_manifest_check(report: &skills::ManifestAuditReport) -> Check {
    let evidence = report.to_json();
    let check = match report.warning() {
        None => Check::ok(
            "skill_manifest",
            CheckGroup::Core,
            "Skill Manifest",
            "findings=0",
        ),
        Some(detail) => Check::warn(
            "skill_manifest",
            CheckGroup::Core,
            "Skill Manifest",
            detail,
            "Review manifest agents and unlisted SKILL.md directories; audit_skipped/inspection_errors mean inspection unavailable.",
        ),
    };
    check.with_subsystem("config_audit").with_evidence(evidence)
}

#[cfg(test)]
#[path = "configuration_audit_tests.rs"]
mod tests;
