use regex::Regex;
use std::sync::{LazyLock, RwLock};
use url::Url;

static AUTH_HEADER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(authorization\s*:\s*(?:[a-z][a-z0-9._~+/-]*\s+)?)[^\r\n]+").unwrap()
});
static ASSIGNMENT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b([A-Z0-9_]*(?:TOKEN|SECRET|PASSWORD|DATABASE_URL|API[_-]?KEY|PRIVATE[_-]?KEY)[A-Z0-9_]*\s*=\s*)[^\s]+")
        .unwrap()
});
// PEM-encoded private keys span multiple whitespace-separated tokens and lines,
// so the single-token ASSIGNMENT_RE value capture (`[^\s]+`) cannot mask the
// whole body. Mask the entire `BEGIN..END ... PRIVATE KEY` block as one unit so
// no portion of the key material survives in prompt/log paths.
static PRIVATE_KEY_BLOCK_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?-----END [A-Z0-9 ]*PRIVATE KEY-----")
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
        // Normalize hyphens to underscores so hyphenated names exported via the
        // `env 'GITHUB-PRIVATE-KEY=...'` form are matched just like underscore names.
        let upper = key.to_ascii_uppercase().replace('-', "_");
        if upper.contains("TOKEN")
            || upper.contains("SECRET")
            || upper.contains("PASSWORD")
            || upper.contains("API_KEY")
            || upper.contains("APIKEY")
            || upper.contains("PRIVATE_KEY")
            || upper.contains("PRIVATEKEY")
        {
            register_secret_or_dsn(&value);
        }
    }
}

pub(crate) fn redact_known_secrets(input: &str) -> String {
    // Mask whole PEM private-key blocks first so later single-token rules cannot
    // leave the key body behind, and so the registered-secret pass is unaffected.
    let redacted = PRIVATE_KEY_BLOCK_RE.replace_all(input, "***");
    let redacted = POSTGRES_DSN_RE.replace_all(&redacted, |captures: &regex::Captures<'_>| {
        mask_dsn_password(captures.get(0).map(|m| m.as_str()).unwrap_or_default())
    });
    let redacted = AUTH_HEADER_RE.replace_all(&redacted, "${1}***");
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
            "Authorization: Bearer live-token\nAuthorization: Bot discord-token\nAuthorization: Basic dXNlcjpwYXNz\nauthorization: Digest username=\"u\", nonce=\"nonce-secret\"\nDATABASE_URL=postgres://u:p@h/db\nOPENAI_API_KEY=sk-live\nGITHUB_PRIVATE_KEY=gh-priv-key-secret\nPRIVATE_KEY=pk-secret",
        );

        assert!(redacted.contains("Authorization: Bearer ***"));
        assert!(redacted.contains("Authorization: Bot ***"));
        assert!(redacted.contains("Authorization: Basic ***"));
        assert!(redacted.contains("authorization: Digest ***"));
        assert!(redacted.contains("DATABASE_URL=***"));
        assert!(redacted.contains("OPENAI_API_KEY=***"));
        assert!(redacted.contains("GITHUB_PRIVATE_KEY=***"));
        assert!(redacted.contains("PRIVATE_KEY=***"));
        assert!(!redacted.contains("live-token"));
        assert!(!redacted.contains("discord-token"));
        assert!(!redacted.contains("dXNlcjpwYXNz"));
        assert!(!redacted.contains("nonce-secret"));
        assert!(!redacted.contains("sk-live"));
        assert!(!redacted.contains("gh-priv-key-secret"));
        assert!(!redacted.contains("pk-secret"));
    }

    #[test]
    fn redact_known_secrets_masks_multiline_pem_private_key_block() {
        let pem = "context before\nGITHUB_PRIVATE_KEY=-----BEGIN RSA PRIVATE KEY-----\nMIIBVAIBADANBgkqhkiG9w0BAQEF\nAASCAj8wggI7AgEAAoIBAQDLEAK=\n-----END RSA PRIVATE KEY-----\ncontext after";
        let redacted = redact_known_secrets(pem);

        // The entire key body and PEM armor must be gone, including interior lines.
        assert!(!redacted.contains("MIIBVAIBADANBgkqhkiG9w0BAQEF"));
        assert!(!redacted.contains("AASCAj8wggI7AgEAAoIBAQDLEAK="));
        assert!(!redacted.contains("BEGIN RSA PRIVATE KEY"));
        assert!(!redacted.contains("END RSA PRIVATE KEY"));
        // Surrounding non-secret context is preserved.
        assert!(redacted.contains("context before"));
        assert!(redacted.contains("context after"));
        assert!(redacted.contains("GITHUB_PRIVATE_KEY=***"));
    }

    #[test]
    fn redact_known_secrets_masks_bare_pem_block_without_assignment() {
        let pem = "ssh failed: -----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAA\n-----END OPENSSH PRIVATE KEY----- (retrying)";
        let redacted = redact_known_secrets(pem);

        assert!(!redacted.contains("b3BlbnNzaC1rZXktdjEAAAAA"));
        assert!(!redacted.contains("OPENSSH PRIVATE KEY"));
        assert!(redacted.contains("ssh failed: ***"));
        assert!(redacted.contains("(retrying)"));
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
