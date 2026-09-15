//! Provider credentials.

use super::*;

pub(super) fn check_claude_cswap_global_conflict() -> Check {
    let catalog = crate::services::discord::org_schema::provider_auth_catalog();
    let named = crate::services::provider_auth_profile::named_claude_profile_ids(&catalog);
    if named.is_empty() {
        return Check::ok(
            "claude_cswap_global_conflict",
            CheckGroup::ProviderRuntime,
            "Claude cswap vs named profiles",
            "No named Claude auth profiles; cswap remains the default/global switch.",
        );
    }
    Check::warn(
        "claude_cswap_global_conflict",
        CheckGroup::ProviderRuntime,
        "Claude cswap vs named profiles",
        format!(
            "Named Claude profiles present ({}). cswap switch stays machine-global for the default home.",
            named.join(", ")
        ),
        "Keep /api/claude-accounts for the default Claude login only. Extra Claude accounts use isolated CLAUDE_CONFIG_DIR homes and must not be switched via cswap.",
    )
    .with_next_steps(vec![
        "Use Settings → Providers extra Claude accounts instead of cswap for named profiles."
            .to_string(),
    ])
    .with_evidence(json!({ "named_claude_profiles": named }))
}

pub(super) fn check_credential_permissions(cfg: &config::Config) -> Check {
    let mut candidates: Vec<(&'static str, PathBuf, bool)> = Vec::new();
    if let Some(root) = config::runtime_root() {
        candidates.push((
            "agentdesk_yaml",
            crate::runtime_layout::config_file_path(&root),
            cfg.server
                .auth_token
                .as_deref()
                .is_some_and(|token| !token.trim().is_empty()),
        ));
        candidates.push((
            "discord_credential_dir",
            crate::runtime_layout::credential_dir(&root),
            true,
        ));
        let mut bot_names = cfg.discord.bots.keys().cloned().collect::<Vec<_>>();
        bot_names.sort();
        for bot_name in bot_names {
            let label = match bot_name.as_str() {
                "command" => "discord_command_token",
                "announce" => "discord_announce_token",
                "notify" => "discord_notify_token",
                _ => "discord_bot_token",
            };
            candidates.push((
                label,
                crate::runtime_layout::credential_token_path(&root, &bot_name),
                true,
            ));
        }
    }
    if let Some(home) = qwen_home_dir() {
        candidates.push((
            "qwen_oauth_cache",
            home.join(".qwen").join("oauth_creds.json"),
            true,
        ));
    }
    if let Some(project) = qwen_project_dir() {
        candidates.push(("qwen_project_env", project.join(".qwen").join(".env"), true));
        candidates.push(("project_env", project.join(".env"), true));
    }

    let findings = candidates
        .iter()
        .map(|(label, path, sensitive)| permission_finding(label, path, *sensitive))
        .collect::<Vec<_>>();
    let risks = findings
        .iter()
        .filter_map(|finding| {
            finding
                .risk
                .as_ref()
                .map(|risk| format!("{}: {risk}", finding.label))
        })
        .collect::<Vec<_>>();
    let existing = findings.iter().filter(|finding| finding.exists).count();
    let evidence = json!({
        "checked": findings.iter().map(|finding| json!({
            "label": finding.label,
            "path": finding.path.clone(),
            "exists": finding.exists,
            "mode": finding.mode.clone(),
            "owner_is_current": finding.owner_is_current,
            "risk": finding.risk.clone(),
        })).collect::<Vec<_>>(),
        "risk_count": risks.len(),
    });
    let detail = format!(
        "checked={} existing={} risks={}",
        findings.len(),
        existing,
        risks.len()
    );
    if risks.is_empty() {
        Check::ok(
            "credential_permissions",
            CheckGroup::ProviderRuntime,
            "Credential Permissions",
            detail.clone(),
        )
        .with_subsystem("security")
        .with_expected_actual("no credential permission risks", detail)
        .with_evidence(evidence)
        .with_security_exposure(SecurityExposure::CredentialMetadata)
    } else {
        Check::warn(
            "credential_permissions",
            CheckGroup::ProviderRuntime,
            "Credential Permissions",
            format!("{detail}; {}", risks.join("; ")),
            "credential/config 파일 내용은 읽거나 출력하지 않고 권한/owner metadata만 점검했습니다.",
        )
        .with_subsystem("security")
        .with_expected_actual("credential files owned by current user with private permissions", detail)
        .with_evidence(evidence)
        .with_security_exposure(SecurityExposure::CredentialMetadata)
        .with_next_steps(vec![
            "chmod 700 ~/.adk/release/credential".to_string(),
            "chmod 600 <credential-file>".to_string(),
        ])
    }
}
