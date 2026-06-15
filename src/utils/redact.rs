use regex::Regex;
use std::sync::{LazyLock, RwLock};
use url::Url;

static AUTH_HEADER_RE: LazyLock<Regex> = LazyLock::new(|| {
    // Horizontal whitespace only (`[ \t]`, never `\s`) so the optional scheme
    // separator cannot consume a newline and mask the FOLLOWING line while
    // leaving a scheme-less value (e.g. `authorization: secret\nnext`) exposed.
    // The value alternation masks RFC 7230 obs-fold continuation lines (which
    // START with horizontal whitespace) as part of the credential: EITHER
    // same-line content + zero-or-more folds, OR one-or-more folds when the
    // first header line is empty (`Authorization:\r\n token`). An ordinary
    // unindented next line is NOT consumed; a value-less header is NOT matched.
    Regex::new(
        r"(?i)\b(authorization[ \t]*:[ \t]*(?:[a-z][a-z0-9._~+/-]*[ \t]+)?)(?:[^\r\n]+(?:\r?\n[ \t]+[^\r\n]+)*|(?:\r?\n[ \t]+[^\r\n]+)+)",
    )
    .unwrap()
});
// Capture group 1 = key (+ optional surrounding `"`/`'` quote) + `=`/`:`
// separator; group 2 = the value, EITHER a quoted string (whole body incl.
// inner spaces, escape-aware so a `\"` inside cannot end the match early and
// leak the tail) OR an unquoted run of non-whitespace. The unquoted branch is
// `\S+` (NOT `[^\s,}]+`): in env/assignment forms a `,` or `}` is part of the
// value (`PASSWORD=abc,def`), so stopping at them left the tail exposed — and
// real JSON/object string values are quoted, so they take the quoted branch and
// keep their `,`/`}` delimiter intact regardless. Handles `K=v`, `k: v`, JSON
// `"k": "v"`, single-quoted `'k': 'v'` dict dumps, and quoted multi-token values.
static ASSIGNMENT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)\b(['"]?[A-Z0-9_-]*(?:TOKEN|SECRET|PASSWORD|DATABASE_URL|API[_-]?KEY|PRIVATE[_-]?KEY)[A-Z0-9_-]*['"]?[ \t]*[:=][ \t]*)("(?:\\.|[^"\\\r\n])*"|'(?:\\.|[^'\\\r\n])*'|\S+)"#)
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
    let mut redacted = ASSIGNMENT_RE
        .replace_all(&redacted, |captures: &regex::Captures<'_>| {
            let key_sep = captures.get(1).map(|m| m.as_str()).unwrap_or_default();
            // `*_ID` / `*-ID` fields (e.g. `private_key_id`, `api-key-id`) are
            // identifiers, not secrets — leave them intact instead of masking a
            // non-secret value.
            let key_name = key_sep
                .trim_end()
                .trim_end_matches([':', '='])
                .trim_end()
                .trim_matches('"')
                .trim_matches('\'');
            if key_name
                .to_ascii_uppercase()
                .replace('-', "_")
                .ends_with("_ID")
            {
                captures
                    .get(0)
                    .map(|m| m.as_str())
                    .unwrap_or_default()
                    .to_string()
            } else {
                format!("{key_sep}***")
            }
        })
        .into_owned();
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
    fn auth_header_scheme_less_value_does_not_leak_into_next_line() {
        // #3440 codex [High]: a scheme-less authorization value followed by a
        // newline must mask the value, not consume the `\n` and expose it.
        let redacted = redact_known_secrets("authorization: plain-secret\nvisible next line");
        assert!(!redacted.contains("plain-secret"), "leaked: {redacted}");
        assert!(redacted.contains("authorization: ***"));
        assert!(redacted.contains("visible next line"));
    }

    #[test]
    fn private_key_colon_json_and_quoted_values_are_masked() {
        // #3440 codex [High]: colon, JSON-quoted-key, and quoted multi-token
        // values were slipping through the `=`-only single-token capture.
        let redacted = redact_known_secrets(
            "private_key: pk-colon-secret\n{\"private_key\": \"pk-json-secret\"}\nPRIVATE_KEY=\"abc def ghi\"\nAPI_KEY = 'quoted-api-secret'",
        );
        assert!(
            !redacted.contains("pk-colon-secret"),
            "colon leak: {redacted}"
        );
        assert!(
            !redacted.contains("pk-json-secret"),
            "json leak: {redacted}"
        );
        assert!(
            !redacted.contains("abc def ghi"),
            "quoted multi-token leak: {redacted}"
        );
        assert!(!redacted.contains("def"), "quoted tail leak: {redacted}");
        assert!(
            !redacted.contains("quoted-api-secret"),
            "single-quote leak: {redacted}"
        );
    }

    #[test]
    fn single_quoted_secret_keys_are_masked() {
        // #3440 codex round 2 [High]: Python/JSON5 dict dumps quote the KEY with
        // `'...'`, which the `"`-only key pattern let slip through entirely.
        let redacted = redact_known_secrets("{'private_key': 'pk-single', 'note': 'keep'}");
        assert!(
            !redacted.contains("pk-single"),
            "single-key leak: {redacted}"
        );
        assert!(
            redacted.contains("keep"),
            "unrelated value dropped: {redacted}"
        );
    }

    #[test]
    fn unquoted_value_with_comma_or_brace_is_fully_masked() {
        // #3440 codex round 5 [High]: in env/assignment forms `,` and `}` are
        // part of the value, so the unquoted branch must consume them rather
        // than stop early and leak the tail.
        let redacted = redact_known_secrets("PASSWORD=abc,def\nPRIVATE_KEY=abc}def");
        assert!(
            !redacted.contains("def"),
            "comma/brace tail leak: {redacted}"
        );
        assert!(
            redacted.contains("PASSWORD=***"),
            "key/sep lost: {redacted}"
        );
        assert!(
            redacted.contains("PRIVATE_KEY=***"),
            "key/sep lost: {redacted}"
        );
        // A genuinely quoted JSON value still keeps its trailing delimiter.
        let json = redact_known_secrets("{\"api_key\": \"abc\", \"id\": 7}");
        assert!(!json.contains("abc"), "json value leak: {json}");
        assert!(json.contains("\"id\": 7"), "delimiter corrupted: {json}");
    }

    #[test]
    fn escaped_quote_in_quoted_value_does_not_leak_tail() {
        // #3440 codex round 2 [High]: an escaped `\"` inside a quoted value ended
        // the non-escape-aware match early, leaking the trailing secret bytes.
        let redacted = redact_known_secrets(r#"PASSWORD="abc\"tail-secret""#);
        assert!(
            !redacted.contains("tail-secret"),
            "escaped-tail leak: {redacted}"
        );
        assert!(
            redacted.contains("PASSWORD=***"),
            "key/sep lost: {redacted}"
        );
    }

    #[test]
    fn folded_auth_header_continuation_is_masked() {
        // #3440 codex round 2 [Medium]: an RFC 7230 obs-fold continuation line
        // (starts with whitespace) carries the wrapped credential and must be
        // masked too; an ordinary unindented next line stays visible.
        let redacted =
            redact_known_secrets("Authorization: Bearer\r\n token-on-continuation\nplain line");
        assert!(
            !redacted.contains("token-on-continuation"),
            "fold leak: {redacted}"
        );
        assert!(
            redacted.contains("plain line"),
            "over-consumed next line: {redacted}"
        );
    }

    #[test]
    fn folded_auth_header_with_empty_first_line_is_masked() {
        // #3440 codex round 3 [High]: an obs-fold header whose first line is
        // empty (`Authorization:\r\n token`) put the whole credential on the
        // continuation line; the `[^\r\n]+`-first value missed it entirely.
        let redacted =
            redact_known_secrets("Authorization:\r\n token-on-empty-first-line\nplain line");
        assert!(
            !redacted.contains("token-on-empty-first-line"),
            "empty-first-line fold leak: {redacted}"
        );
        assert!(
            redacted.contains("plain line"),
            "over-consumed next line: {redacted}"
        );
    }

    #[test]
    fn identifier_id_fields_are_not_over_redacted() {
        // #3440 codex [Low]: `*_ID` / `*-ID` are identifiers, not secrets.
        let redacted = redact_known_secrets("private_key_id=not-secret-id api-key-id: visible-id");
        assert!(
            redacted.contains("not-secret-id"),
            "over-redacted: {redacted}"
        );
        assert!(redacted.contains("visible-id"), "over-redacted: {redacted}");
        // ...but a real PRIVATE_KEY adjacent to an _ID field is still masked.
        let mixed = redact_known_secrets("PRIVATE_KEY_ID=keep-id PRIVATE_KEY=mask-me");
        assert!(mixed.contains("keep-id"));
        assert!(!mixed.contains("mask-me"), "real key leaked: {mixed}");
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
