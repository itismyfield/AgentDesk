use std::path::PathBuf;

use serde_json::json;

use super::{Check, CheckGroup, SecurityExposure};

pub(super) fn qwen_home_dir() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("QWEN_HOME") {
        if !path.is_empty() {
            return Some(PathBuf::from(path));
        }
    }

    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        })
        .or_else(dirs::home_dir)
}

pub(super) fn qwen_project_dir() -> Option<PathBuf> {
    std::env::current_dir().ok()
}

fn qwen_system_defaults_path() -> Option<PathBuf> {
    std::env::var_os("QWEN_CODE_SYSTEM_DEFAULTS_PATH")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
}

fn qwen_system_settings_path() -> Option<PathBuf> {
    std::env::var_os("QWEN_CODE_SYSTEM_SETTINGS_PATH")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
}

fn qwen_user_settings_path() -> Option<PathBuf> {
    qwen_home_dir().map(|home| home.join(".qwen").join("settings.json"))
}

fn qwen_project_settings_path() -> Option<PathBuf> {
    qwen_project_dir().map(|dir| dir.join(".qwen").join("settings.json"))
}

fn format_artifact_scope(scope: &str, items: &[&str]) -> String {
    if items.is_empty() {
        format!("{scope}: -")
    } else {
        format!("{scope}: {}", items.join(", "))
    }
}

pub(super) fn check_qwen_settings_files(configured: bool) -> Check {
    let candidates = [
        ("system defaults", qwen_system_defaults_path()),
        ("user settings", qwen_user_settings_path()),
        ("project settings", qwen_project_settings_path()),
        ("system settings", qwen_system_settings_path()),
    ];
    let found: Vec<String> = candidates
        .iter()
        .filter_map(|(label, path)| {
            path.as_ref()
                .filter(|path| path.is_file())
                .map(|path| format!("{label}={}", path.display()))
        })
        .collect();

    if !found.is_empty() {
        return Check::ok(
            "provider_qwen_settings",
            CheckGroup::ProviderRuntime,
            "qwen settings files",
            format!("found {}", found.join(" | ")),
        )
        .with_path(found[0].clone())
        .with_expected_actual(
            "Qwen settings layers discoverable",
            format!("{} settings layer(s) found", found.len()),
        );
    }

    let guidance = "Qwen can operate without settings, but for reliable model picker and operational surface usage, configuring ~/.qwen/settings.json or <workspace>/.qwen/settings.json is recommended.";
    if configured {
        Check::warn(
            "provider_qwen_settings",
            CheckGroup::ProviderRuntime,
            "qwen settings files",
            "no Qwen settings files detected",
            guidance,
        )
        .with_expected_actual(
            "at least one Qwen settings layer",
            "no settings.json detected",
        )
        .with_next_steps(vec![
            "ls -la ~/.qwen".to_string(),
            "ls -la ./.qwen".to_string(),
        ])
    } else {
        Check::ok(
            "provider_qwen_settings",
            CheckGroup::ProviderRuntime,
            "qwen settings files",
            "no Qwen settings files detected (provider not configured)",
        )
        .with_expected_actual(
            "settings present if qwen is actively used",
            "qwen not configured",
        )
    }
}

pub(super) fn check_qwen_auth_hints(configured: bool) -> Check {
    let home = qwen_home_dir();
    let project = qwen_project_dir();
    let oauth_cache = home
        .as_ref()
        .map(|path| path.join(".qwen").join("oauth_creds.json"));
    let project_qwen_env = project.as_ref().map(|path| path.join(".qwen").join(".env"));
    let project_env = project.as_ref().map(|path| path.join(".env"));

    let mut hints = Vec::new();
    if oauth_cache.as_ref().is_some_and(|path| path.is_file()) {
        hints.push("cached OAuth");
    }
    if project_qwen_env.as_ref().is_some_and(|path| path.is_file()) {
        hints.push("project .qwen/.env");
    }
    if project_env.as_ref().is_some_and(|path| path.is_file()) {
        hints.push("project .env");
    }

    let detail = if hints.is_empty() {
        "interactive: OAuth or API key | headless: cached auth or API key only".to_string()
    } else {
        format!(
            "interactive: OAuth or API key | headless: cached auth or API key only | found: {}",
            hints.join(", ")
        )
    };

    if !hints.is_empty() {
        return Check::ok(
            "provider_qwen_auth",
            CheckGroup::ProviderRuntime,
            "qwen auth hints",
            detail,
        )
        .with_expected_actual("cached auth or API-key hint visible", hints.join(", "))
        .with_next_steps(vec![
            "qwen auth status".to_string(),
            "Check DashScope web console for usage limits".to_string(),
        ]);
    }

    let guidance = "Check the API key path in the project .qwen/.env first, then .env. The Qwen CLI does not merge env-files. Check DashScope web console or official documentation for usage/limits instead of hardcoding numbers in doctor.";
    if configured {
        Check::warn(
            "provider_qwen_auth",
            CheckGroup::ProviderRuntime,
            "qwen auth hints",
            detail,
            guidance,
        )
        .with_expected_actual(
            "cached auth or API-key hint visible",
            "no oauth cache or env hints found",
        )
        .with_next_steps(vec![
            "qwen auth status".to_string(),
            "ls -la ./.qwen".to_string(),
        ])
    } else {
        Check::ok(
            "provider_qwen_auth",
            CheckGroup::ProviderRuntime,
            "qwen auth hints",
            format!("{detail} (provider not configured)"),
        )
        .with_expected_actual(
            "auth hint required only if qwen is used",
            "qwen not configured",
        )
    }
}

pub(super) fn check_qwen_runtime_artifacts(configured: bool) -> Check {
    let home = qwen_home_dir();
    let project = qwen_project_dir();

    let home_artifacts = [
        (
            "extensions",
            home.as_ref()
                .map(|path| path.join(".qwen").join("extensions")),
            false,
        ),
        (
            "commands",
            home.as_ref()
                .map(|path| path.join(".qwen").join("commands")),
            false,
        ),
        (
            "agents",
            home.as_ref().map(|path| path.join(".qwen").join("agents")),
            false,
        ),
        (
            "skills",
            home.as_ref().map(|path| path.join(".qwen").join("skills")),
            false,
        ),
        (
            "output-language.md",
            home.as_ref()
                .map(|path| path.join(".qwen").join("output-language.md")),
            true,
        ),
    ];
    let project_artifacts = [
        (
            "commands",
            project
                .as_ref()
                .map(|path| path.join(".qwen").join("commands")),
            false,
        ),
        (
            "agents",
            project
                .as_ref()
                .map(|path| path.join(".qwen").join("agents")),
            false,
        ),
        (
            "skills",
            project
                .as_ref()
                .map(|path| path.join(".qwen").join("skills")),
            false,
        ),
        (
            "PROJECT_SUMMARY.md",
            project
                .as_ref()
                .map(|path| path.join(".qwen").join("PROJECT_SUMMARY.md")),
            true,
        ),
        (
            "settings.json",
            project
                .as_ref()
                .map(|path| path.join(".qwen").join("settings.json")),
            true,
        ),
        (
            ".qwen/.env",
            project.as_ref().map(|path| path.join(".qwen").join(".env")),
            true,
        ),
        (".env", project.as_ref().map(|path| path.join(".env")), true),
    ];

    let found_home: Vec<&str> = home_artifacts
        .iter()
        .filter_map(|(label, path, is_file)| {
            path.as_ref().and_then(|path| {
                let exists = if *is_file {
                    path.is_file()
                } else {
                    path.is_dir()
                };
                exists.then_some(*label)
            })
        })
        .collect();
    let found_project: Vec<&str> = project_artifacts
        .iter()
        .filter_map(|(label, path, is_file)| {
            path.as_ref().and_then(|path| {
                let exists = if *is_file {
                    path.is_file()
                } else {
                    path.is_dir()
                };
                exists.then_some(*label)
            })
        })
        .collect();

    let detail = format!(
        "{} | {}",
        format_artifact_scope("home", &found_home),
        format_artifact_scope("project", &found_project)
    );

    if !found_home.is_empty() || !found_project.is_empty() {
        return Check::ok(
            "provider_qwen_runtime",
            CheckGroup::ProviderRuntime,
            "qwen runtime artifacts",
            detail,
        )
        .with_expected_actual(
            "Qwen runtime artifacts visible when configured",
            format!("home={} project={}", found_home.len(), found_project.len()),
        );
    }

    let guidance = "Qwen은 ~/.qwen/extensions, ~/.qwen/skills, <workspace>/.qwen/PROJECT_SUMMARY.md, <workspace>/.qwen/.env 같은 로컬 자산을 그대로 사용합니다. headless 환경에서는 project .qwen/.env 우선 여부를 함께 확인하세요.";
    if configured {
        Check::warn(
            "provider_qwen_runtime",
            CheckGroup::ProviderRuntime,
            "qwen runtime artifacts",
            detail,
            guidance,
        )
        .with_expected_actual(
            "at least one Qwen runtime artifact when heavily customized",
            "no Qwen runtime artifacts detected",
        )
        .with_next_steps(vec![
            "ls -la ~/.qwen".to_string(),
            "ls -la ./.qwen".to_string(),
        ])
    } else {
        Check::ok(
            "provider_qwen_runtime",
            CheckGroup::ProviderRuntime,
            "qwen runtime artifacts",
            format!("{detail} (provider not configured)"),
        )
        .with_expected_actual(
            "runtime artifacts required only if qwen is used",
            "qwen not configured",
        )
    }
}
