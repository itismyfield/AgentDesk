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
    prompt_api_routes_check_with_timeout(repo, prompts, std::time::Duration::from_secs(10))
}

fn prompt_api_routes_check_with_timeout(
    repo: &Path,
    prompts: &Path,
    timeout: std::time::Duration,
) -> Check {
    // Invoke deploy staging's checker from the same checkout as its inventory.
    let output = std::process::Command::new("python3")
        .arg(repo.join("scripts/check_prompt_api_routes.py"))
        .arg("--repo")
        .arg(repo)
        .arg("--prompts")
        .arg(prompts)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn();
    let (code, detail) = match output {
        Ok(mut child) => {
            let Some(mut stdout) = child.stdout.take() else {
                let _ = child.kill();
                let _ = child.wait();
                return prompt_result(None, "inspection_unavailable: stdout".into());
            };
            let reader = std::thread::spawn(move || {
                use std::io::Read;
                let mut bytes = Vec::new();
                stdout.read_to_end(&mut bytes).map(|_| bytes)
            });
            let started = std::time::Instant::now();
            let outcome = loop {
                match child.try_wait() {
                    Ok(Some(status)) => break Ok(status),
                    Ok(None) if started.elapsed() < timeout => {
                        std::thread::sleep(std::time::Duration::from_millis(10))
                    }
                    result => {
                        let _ = child.kill();
                        let _ = child.wait();
                        break Err(match result {
                            Err(error) => format!("inspection_unavailable: {error}"),
                            _ => "inspection_unavailable: timeout".into(),
                        });
                    }
                }
            };
            let bytes = reader.join().ok().and_then(Result::ok);
            match (outcome, bytes) {
                (Ok(status), Some(bytes)) => (
                    status.code(),
                    String::from_utf8_lossy(&bytes).trim().to_string(),
                ),
                (Err(error), _) => (None, error),
                _ => (None, "inspection_unavailable: stdout read".into()),
            }
        }
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
        Some(_) => Check::warn(
            "skill_manifest",
            CheckGroup::Core,
            "Skill Manifest",
            format!("skill manifest audit: {evidence}"),
            "Review manifest agents and unlisted SKILL.md directories; audit_skipped/inspection_errors mean inspection unavailable.",
        ),
    };
    check.with_subsystem("config_audit").with_evidence(evidence)
}

#[cfg(test)]
#[path = "configuration_audit_tests.rs"]
mod tests;
