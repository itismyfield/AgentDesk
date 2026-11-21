use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::super::DiscordTokenMode;
use super::super::source::{
    OpenClawSecretProviderConfig, OpenClawSecretsConfig, ResolvedSourceRoot,
};

pub(super) fn resolve_account_token(
    source: &ResolvedSourceRoot,
    account_id: &str,
    mode: DiscordTokenMode,
) -> Result<Option<String>, String> {
    let Some(discord) = source.config.channels.discord.as_ref() else {
        return Ok(None);
    };
    let token_value = if account_id == "default" {
        discord.token.as_ref()
    } else {
        discord
            .accounts
            .get(account_id)
            .and_then(|account| account.token.as_ref())
    };
    let Some(token_value) = token_value else {
        return Ok(None);
    };
    resolve_token_value(
        token_value,
        source.config.secrets.as_ref(),
        &source.root,
        mode,
    )
    .map(Some)
}

#[derive(Clone, Debug)]
struct SecretRefSpec {
    source: String,
    provider: String,
    id: String,
    implicit_provider: bool,
}

fn resolve_token_value(
    token_value: &serde_json::Value,
    secrets: Option<&OpenClawSecretsConfig>,
    source_root: &Path,
    mode: DiscordTokenMode,
) -> Result<String, String> {
    if let Some(value) = token_value.as_str() {
        let trimmed = value.trim();
        if !(trimmed.starts_with("${") && trimmed.ends_with('}')) {
            return Ok(trimmed.to_string());
        }
    }

    let secret_ref = parse_secret_ref(token_value, secrets)?;
    let Some(secret_ref) = secret_ref else {
        return Err("unsupported Discord token shape".to_string());
    };

    match (mode, secret_ref.source.as_str()) {
        (DiscordTokenMode::PlaintextOnly, _) => {
            Err("token mode plaintext-only does not allow SecretRef tokens".to_string())
        }
        (DiscordTokenMode::ResolveEnvFile, "exec") => {
            Err("token mode resolve-env-file does not allow exec SecretRefs".to_string())
        }
        (DiscordTokenMode::Report, _) => {
            Err("token mode report does not resolve tokens".to_string())
        }
        _ => resolve_secret_ref_value(&secret_ref, secrets, source_root),
    }
}

fn parse_secret_ref(
    token_value: &serde_json::Value,
    secrets: Option<&OpenClawSecretsConfig>,
) -> Result<Option<SecretRefSpec>, String> {
    if let Some(raw) = token_value.as_str() {
        let trimmed = raw.trim();
        if let Some(name) = trimmed
            .strip_prefix("${")
            .and_then(|value| value.strip_suffix('}'))
        {
            let provider = secrets
                .and_then(|secrets| secrets.defaults.env.clone())
                .unwrap_or_else(|| "default".to_string());
            return Ok(Some(SecretRefSpec {
                source: "env".to_string(),
                provider,
                id: name.to_string(),
                implicit_provider: secrets
                    .and_then(|value| value.defaults.env.as_ref())
                    .is_none(),
            }));
        }
        return Ok(None);
    }
    let Some(object) = token_value.as_object() else {
        return Ok(None);
    };
    let Some(source) = object
        .get("source")
        .and_then(serde_json::Value::as_str)
        .map(|value| value.trim().to_ascii_lowercase())
    else {
        return Ok(None);
    };
    let Some(id) = object
        .get("id")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Err("SecretRef token is missing id".to_string());
    };
    let implicit_provider = object.get("provider").is_none()
        && match source.as_str() {
            "env" => secrets
                .and_then(|secrets| secrets.defaults.env.as_ref())
                .is_none(),
            _ => false,
        };
    let provider = object
        .get("provider")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| match source.as_str() {
            "env" => secrets.and_then(|secrets| secrets.defaults.env.clone()),
            "file" => secrets.and_then(|secrets| secrets.defaults.file.clone()),
            "exec" => secrets.and_then(|secrets| secrets.defaults.exec.clone()),
            _ => None,
        })
        .unwrap_or_else(|| "default".to_string());
    Ok(Some(SecretRefSpec {
        source,
        provider,
        id: id.to_string(),
        implicit_provider,
    }))
}

fn resolve_secret_ref_value(
    secret_ref: &SecretRefSpec,
    secrets: Option<&OpenClawSecretsConfig>,
    source_root: &Path,
) -> Result<String, String> {
    if secret_ref.source == "env" && secret_ref.implicit_provider {
        return std::env::var(&secret_ref.id).map_err(|_| {
            format!(
                "Env SecretRef '{}' is not set in the current environment.",
                secret_ref.id
            )
        });
    }

    let provider = secrets
        .and_then(|secrets| secrets.providers.get(&secret_ref.provider))
        .ok_or_else(|| {
            format!(
                "Missing OpenClaw secrets provider '{}' for {} SecretRef.",
                secret_ref.provider, secret_ref.source
            )
        })?;

    match provider {
        OpenClawSecretProviderConfig::Env { allowlist } => {
            if !allowlist.is_empty() && !allowlist.iter().any(|item| item == &secret_ref.id) {
                return Err(format!(
                    "Env SecretRef '{}' is not in allowlist for provider '{}'.",
                    secret_ref.id, secret_ref.provider
                ));
            }
            std::env::var(&secret_ref.id).map_err(|_| {
                format!(
                    "Env SecretRef '{}' is not set in the current environment.",
                    secret_ref.id
                )
            })
        }
        OpenClawSecretProviderConfig::File {
            path,
            mode,
            max_bytes,
            ..
        } => {
            let resolved_path = {
                let path = PathBuf::from(path);
                if path.is_absolute() {
                    path
                } else {
                    source_root.join(path)
                }
            };
            let bytes = fs::read(&resolved_path).map_err(|e| {
                format!(
                    "Failed to read SecretRef file provider '{}' at '{}': {e}",
                    secret_ref.provider,
                    resolved_path.display()
                )
            })?;
            if let Some(limit) = max_bytes {
                if bytes.len() as u64 > *limit {
                    return Err(format!(
                        "SecretRef file provider '{}' exceeded maxBytes.",
                        secret_ref.provider
                    ));
                }
            }
            let raw = String::from_utf8(bytes).map_err(|e| {
                format!(
                    "SecretRef file '{}' is not UTF-8: {e}",
                    resolved_path.display()
                )
            })?;
            if mode.as_deref() == Some("json") {
                let value: serde_json::Value = serde_json::from_str(&raw).map_err(|e| {
                    format!(
                        "Failed to parse JSON SecretRef file '{}': {e}",
                        resolved_path.display()
                    )
                })?;
                extract_secret_json_value(&value, &secret_ref.id)
            } else {
                Ok(raw.trim().to_string())
            }
        }
        OpenClawSecretProviderConfig::Exec {
            command,
            args,
            json_only,
            env,
            pass_env,
            ..
        } => {
            let mut cmd = Command::new(command);
            cmd.args(args);
            cmd.stdin(Stdio::null());
            cmd.stderr(Stdio::piped());
            cmd.stdout(Stdio::piped());
            for (key, value) in env {
                cmd.env(key, value);
            }
            for key in pass_env {
                if let Ok(value) = std::env::var(key) {
                    cmd.env(key, value);
                }
            }
            let output = cmd.output().map_err(|e| {
                format!(
                    "Failed to execute SecretRef provider '{}': {e}",
                    secret_ref.provider
                )
            })?;
            if !output.status.success() {
                return Err(format!(
                    "SecretRef exec provider '{}' failed: {}",
                    secret_ref.provider,
                    String::from_utf8_lossy(&output.stderr).trim()
                ));
            }
            let stdout = String::from_utf8(output.stdout).map_err(|e| {
                format!(
                    "SecretRef exec provider '{}' returned non UTF-8 output: {e}",
                    secret_ref.provider
                )
            })?;
            let trimmed = stdout.trim();
            if json_only == &Some(true) || trimmed.starts_with('{') || trimmed.starts_with('[') {
                let value: serde_json::Value = serde_json::from_str(trimmed).map_err(|e| {
                    format!(
                        "SecretRef exec provider '{}' returned invalid JSON: {e}",
                        secret_ref.provider
                    )
                })?;
                extract_secret_json_value(&value, &secret_ref.id)
            } else {
                Ok(trimmed.to_string())
            }
        }
    }
}

fn extract_secret_json_value(value: &serde_json::Value, id: &str) -> Result<String, String> {
    if let Some(pointer_value) = value.pointer(id).and_then(serde_json::Value::as_str) {
        return Ok(pointer_value.to_string());
    }
    if let Some(object_value) = value.get(id).and_then(serde_json::Value::as_str) {
        return Ok(object_value.to_string());
    }
    if let Some(text) = value.as_str() {
        return Ok(text.to_string());
    }
    Err(format!(
        "SecretRef JSON payload did not contain string id '{}'.",
        id
    ))
}
