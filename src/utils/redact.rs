use regex::Regex;
use std::sync::{LazyLock, RwLock};
use url::Url;

static BEARER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(Authorization:\s*(?:Bearer|Bot)\s+)[^\s]+").unwrap());
static ASSIGNMENT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b([A-Z0-9_]*(?:TOKEN|SECRET|PASSWORD|DATABASE_URL|API[_-]?KEY)[A-Z0-9_]*\s*=\s*)[^\s]+")
        .unwrap()
});
static POSTGRES_DSN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"postgres(?:ql)?://[^\s]+").unwrap());
static KNOWN_SECRETS: LazyLock<RwLock<Vec<String>>> = LazyLock::new(|| RwLock::new(Vec::new()));

const MIN_REGISTERED_SECRET_LEN: usize = 6;

pub(crate) fn mask_dsn_password(input: &str) -> String {
    let Ok(mut url) = Url::parse(input) else {
        return input.to_string();
    };
    if !matches!(url.scheme(), "postgres" | "postgresql") || url.password().is_none() {
        return input.to_string();
    }
    let _ = url.set_password(Some("***"));
    url.to_string()
}

fn dsn_password(input: &str) -> Option<String> {
    let url = Url::parse(input).ok()?;
    if !matches!(url.scheme(), "postgres" | "postgresql") {
        return None;
    }
    url.password()
        .filter(|password| !password.trim().is_empty())
        .map(ToString::to_string)
}

pub(crate) fn register_known_secret(secret: &str) {
    let secret = secret.trim();
    if secret.len() < MIN_REGISTERED_SECRET_LEN || secret == "***" {
        return;
    }
    let mut guard = match KNOWN_SECRETS.write() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    if !guard.iter().any(|existing| existing == secret) {
        guard.push(secret.to_string());
        guard.sort_by_key(|value| std::cmp::Reverse(value.len()));
    }
}

pub(crate) fn register_secret_or_dsn(value: &str) {
    if let Some(password) = dsn_password(value) {
        register_known_secret(&password);
    }
    register_known_secret(value);
}

pub(crate) fn register_common_env_secrets() {
    for key in [
        "DATABASE_URL",
        "POSTGRES_TEST_DATABASE_URL_BASE",
        "DISCORD_TOKEN",
        "AGENTDESK_AUTH_TOKEN",
        "ANTHROPIC_API_KEY",
        "OPENAI_API_KEY",
        "MEMENTO_ACCESS_KEY",
        "MEMENTO_MCP_TOKEN",
    ] {
        if let Ok(value) = std::env::var(key) {
            register_secret_or_dsn(&value);
        }
    }

    for (key, value) in std::env::vars() {
        let upper = key.to_ascii_uppercase();
        if upper.contains("TOKEN")
            || upper.contains("SECRET")
            || upper.contains("PASSWORD")
            || upper.contains("API_KEY")
            || upper.contains("APIKEY")
        {
            register_secret_or_dsn(&value);
        }
    }
}

pub(crate) fn redact_known_secrets(input: &str) -> String {
    let redacted = POSTGRES_DSN_RE.replace_all(input, |captures: &regex::Captures<'_>| {
        mask_dsn_password(captures.get(0).map(|m| m.as_str()).unwrap_or_default())
    });
    let redacted = BEARER_RE.replace_all(&redacted, "${1}***");
    let mut redacted = ASSIGNMENT_RE.replace_all(&redacted, "${1}***").into_owned();
    let secrets = match KNOWN_SECRETS.read() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    };
    for secret in secrets {
        if redacted.contains(&secret) {
            redacted = redacted.replace(&secret, "***");
        }
    }
    redacted
}

#[cfg(test)]
mod tests {
    use super::{
        dsn_password, mask_dsn_password, redact_known_secrets, register_known_secret,
        register_secret_or_dsn,
    };

    fn redact_known_secret(input: &str) -> String {
        redact_known_secrets(input)
    }

    #[test]
    fn mask_dsn_password_redacts_postgres_password() {
        assert_eq!(
            mask_dsn_password("postgres://agent:secret@db.internal:5432/agentdesk"),
            "postgres://agent:***@db.internal:5432/agentdesk"
        );
    }

    #[test]
    fn redact_known_secrets_masks_bearer_bot_and_assignments() {
        let redacted = redact_known_secrets(
            "Authorization: Bearer live-token\nAuthorization: Bot discord-token\nDATABASE_URL=postgres://u:p@h/db\nOPENAI_API_KEY=sk-live",
        );

        assert!(redacted.contains("Authorization: Bearer ***"));
        assert!(redacted.contains("Authorization: Bot ***"));
        assert!(redacted.contains("DATABASE_URL=***"));
        assert!(redacted.contains("OPENAI_API_KEY=***"));
        assert!(!redacted.contains("live-token"));
        assert!(!redacted.contains("discord-token"));
        assert!(!redacted.contains("sk-live"));
    }

    #[test]
    fn registered_secret_is_redacted_from_plain_errors() {
        register_known_secret("plain-live-secret");
        let redacted = redact_known_secret("sqlx failed with plain-live-secret in detail");
        assert_eq!(redacted, "sqlx failed with *** in detail");
    }

    #[test]
    fn register_secret_or_dsn_also_masks_dsn_password() {
        register_secret_or_dsn("postgres://agent:dsn-secret@db.internal/agentdesk");
        let redacted = redact_known_secret("password dsn-secret leaked outside url");
        assert_eq!(redacted, "password *** leaked outside url");
    }

    #[test]
    fn dsn_password_extracts_postgres_password_only() {
        assert_eq!(
            dsn_password("postgresql://user:pass@localhost/db").as_deref(),
            Some("pass")
        );
        assert_eq!(dsn_password("https://user:pass@example.test"), None);
    }
}
