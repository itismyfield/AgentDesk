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
static COOKIE_HEADER_RE: LazyLock<Regex> = LazyLock::new(|| {
    // Cookie values have no authentication-scheme prefix. Preserve only the
    // header name, colon, and surrounding horizontal whitespace, then mask the
    // entire value. Reusing the Authorization prefix rule here would leak the
    // first whitespace-delimited cookie token as a false "scheme".
    //
    // Match the same RFC 7230 obs-fold shapes as AUTH_HEADER_RE while leaving
    // an ordinary unindented following line untouched.
    // The leading boundary rejects every RFC `tchar`, preventing a suffix
    // match inside distinct names such as `X-Cookie` or `CookieJar`. Quoted
    // header arguments are handled by quote-aware rules below so the closing
    // delimiter and following shell command are not consumed as header value.
    // Multiline mode lets `^` recognize each header line.
    Regex::new(
        r#"(?im)((?:^|[^"!#$%&'*+.^_`|~a-z0-9=\-\r\n])(?:set-cookie|cookie)[ \t]*:[ \t]*)(?:[^\r\n]+(?:\r?\n[ \t]+[^\r\n]+)*|(?:\r?\n[ \t]+[^\r\n]+)+)"#,
    )
    .unwrap()
});
const HEADER_OPTION_PATTERN: &str =
    r"(?:-[A-Za-z]*?H[ \t]*(?:\\\r?\n[ \t]*)*|(?i:--header)(?:[ \t]+|=[ \t]*)(?:\\\r?\n[ \t]*)*)";
const CURL_COOKIE_OPTION_PATTERN: &str =
    r"(?:-[A-Za-z]*?b[ \t]*(?:\\\r?\n[ \t]*)*|(?i:--cookie)(?:[ \t]+|=[ \t]*)(?:\\\r?\n[ \t]*)*)";
// A shell word can concatenate unquoted, single-quoted, double-quoted, and
// ANSI-C-quoted fragments without whitespace. Backslash-newline is part of the
// same logical word, and quoted fragments may contain literal newlines. Keep
// this grammar shared so header and curl-cookie options cannot drift apart.
const SHELL_WORD_FRAGMENT_PATTERN: &str = r#"(?:\\(?:\r?\n[ \t]*|[^\r\n])|[^ \t\r\n;&|()<>'"$\\]|'[^']*'|"(?:\\(?:\r?\n|[^\r\n])|[^"\\])*"|\$'(?:\\(?:\r?\n|[^\r\n])|[^'\\])*')"#;

static SINGLE_QUOTED_COOKIE_HEADER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?m)((?:^|[ \t;&|()<>])(?:{})?'(?i:set-cookie|cookie)[ \t]*:[ \t]*)[^']*('?)(?:{})*",
        HEADER_OPTION_PATTERN, SHELL_WORD_FRAGMENT_PATTERN
    ))
    .unwrap()
});
static ANSI_C_QUOTED_COOKIE_HEADER_RE: LazyLock<Regex> = LazyLock::new(|| {
    // Bash ANSI-C strings use `$'…'` and allow backslash escapes, including an
    // escaped quote inside the header value. Keep this grammar separate from
    // ordinary single quotes, where backslashes have no escape semantics.
    Regex::new(&format!(
        r"(?m)((?:^|[ \t;&|()<>])(?:{})?\$'(?i:set-cookie|cookie)[ \t]*:[ \t]*)(?:\\(?:\r?\n|[^\r\n])|[^'\\])*('?)(?:{})*",
        HEADER_OPTION_PATTERN, SHELL_WORD_FRAGMENT_PATTERN
    ))
    .unwrap()
});
static DOUBLE_QUOTED_COOKIE_HEADER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r#"(?m)((?:^|[ \t;&|()<>])(?:{})?"(?i:set-cookie|cookie)[ \t]*:[ \t]*)(?:\\(?:\r?\n|[^\r\n])|[^"\\])*("?)(?:{})*"#,
        HEADER_OPTION_PATTERN, SHELL_WORD_FRAGMENT_PATTERN
    ))
    .unwrap()
});
static COMMAND_UNQUOTED_COOKIE_HEADER_RE: LazyLock<Regex> = LazyLock::new(|| {
    // curl accepts attached/clustered -H arguments and curl/wget share the
    // long --header spelling. Keep this shell-word grammar separate from
    // COOKIE_HEADER_RE: a real header extends to end-of-line and may obs-fold,
    // while an unquoted command argument ends at an unescaped shell boundary.
    Regex::new(&format!(
        r#"(?m)((?:^|[ \t;&|()<>]){}(?i:set-cookie|cookie)[ \t]*:[ \t]*)(?:{})+"#,
        HEADER_OPTION_PATTERN, SHELL_WORD_FRAGMENT_PATTERN
    ))
    .unwrap()
});
static SHELL_COOKIE_HEADER_OPTION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r#"(?m)(?:^|[ \t;&|()<>]){}(?:\$?'|")?(?i:set-cookie|cookie)[ \t]*:"#,
        HEADER_OPTION_PATTERN
    ))
    .unwrap()
});
static CURL_COMMAND_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(?:^|[ \t;&|()<>/'"\\])curl(?:\.exe)?(?:$|[ \t;&|()<>/'"\\])"#).unwrap()
});
static CURL_COOKIE_OPTION_PREFIX_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?m)(?:^|[ \t;&|()<>]){}",
        CURL_COOKIE_OPTION_PATTERN
    ))
    .unwrap()
});
static CURL_COOKIE_OPTION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r#"(?m)((?:^|[ \t;&|()<>]){})(?:\\[^\r\n]|[^ \t\r\n;&|()<>'"$\\])(?:{})*"#,
        CURL_COOKIE_OPTION_PATTERN, SHELL_WORD_FRAGMENT_PATTERN
    ))
    .unwrap()
});
static SINGLE_QUOTED_CURL_COOKIE_OPTION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?m)((?:^|[ \t;&|()<>]){}')[^']*('?)(?:{})*",
        CURL_COOKIE_OPTION_PATTERN, SHELL_WORD_FRAGMENT_PATTERN
    ))
    .unwrap()
});
static ANSI_C_QUOTED_CURL_COOKIE_OPTION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?m)((?:^|[ \t;&|()<>]){}\$')(?:\\(?:\r?\n|[^\r\n])|[^'\\])*('?)(?:{})*",
        CURL_COOKIE_OPTION_PATTERN, SHELL_WORD_FRAGMENT_PATTERN
    ))
    .unwrap()
});
static DOUBLE_QUOTED_CURL_COOKIE_OPTION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r#"(?m)((?:^|[ \t;&|()<>]){}")(?:\\(?:\r?\n|[^\r\n])|[^"\\])*("?)(?:{})*"#,
        CURL_COOKIE_OPTION_PATTERN, SHELL_WORD_FRAGMENT_PATTERN
    ))
    .unwrap()
});
static SENSITIVE_HEADER_PREFIX_RE: LazyLock<Regex> = LazyLock::new(|| {
    // Prefix-only detector for a relay tail that begins mid-line. It never
    // materializes the matched value; callers use it only to decide whether an
    // already-bounded partial line must be dropped. Quotes are accepted as
    // shell delimiters, including clustered/long curl header options, so
    // `X-Cookie` remains distinct from a sensitive header name.
    Regex::new(&format!(
        r#"(?m)(?:^|[^!#$%&*+.^_`|~a-z0-9\-])(?:(?i:authorization|set-cookie|cookie)[ \t]*:|{}(?:\$?'|")?(?i:set-cookie|cookie)[ \t]*:)"#,
        HEADER_OPTION_PATTERN
    ))
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

fn replace_header_value(input: &str, regex: &Regex, marker: &str) -> String {
    regex
        .replace_all(input, |captures: &regex::Captures<'_>| {
            let prefix = captures.get(1).map(|m| m.as_str()).unwrap_or_default();
            format!("{prefix}{marker}")
        })
        .into_owned()
}

fn replace_quoted_header_value(input: &str, regex: &Regex, marker: &str) -> String {
    regex
        .replace_all(input, |captures: &regex::Captures<'_>| {
            let prefix = captures.get(1).map(|m| m.as_str()).unwrap_or_default();
            let closing_quote = captures.get(2).map(|m| m.as_str()).unwrap_or_default();
            format!("{prefix}{marker}{closing_quote}")
        })
        .into_owned()
}

fn shell_line_continues(line: &str) -> bool {
    let trimmed = line.trim_end_matches(['\r', '\n']).trim_end();
    trimmed
        .as_bytes()
        .iter()
        .rev()
        .take_while(|&&byte| byte == b'\\')
        .count()
        % 2
        == 1
}

fn redact_curl_cookie_option_chunk(input: &str, marker: &str) -> String {
    let redacted = replace_quoted_header_value(input, &ANSI_C_QUOTED_CURL_COOKIE_OPTION_RE, marker);
    let redacted =
        replace_quoted_header_value(&redacted, &SINGLE_QUOTED_CURL_COOKIE_OPTION_RE, marker);
    let redacted =
        replace_quoted_header_value(&redacted, &DOUBLE_QUOTED_CURL_COOKIE_OPTION_RE, marker);
    replace_header_value(&redacted, &CURL_COOKIE_OPTION_RE, marker)
}

fn redact_curl_cookie_options(input: &str, marker: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut logical_command = String::new();
    let mut contains_curl = false;

    let flush = |output: &mut String, command: &mut String, contains_curl: bool| {
        if contains_curl {
            output.push_str(&redact_curl_cookie_option_chunk(command, marker));
        } else {
            output.push_str(command);
        }
        command.clear();
    };

    for line in input.split_inclusive('\n') {
        contains_curl |= CURL_COMMAND_RE.is_match(line);
        logical_command.push_str(line);
        if shell_line_continues(line) {
            continue;
        }

        flush(&mut output, &mut logical_command, contains_curl);
        contains_curl = false;
    }

    if !logical_command.is_empty() {
        flush(&mut output, &mut logical_command, contains_curl);
    }

    output
}

fn replace_generic_cookie_headers(input: &str, marker: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut cursor = 0;

    // The specialized passes have already sanitized curl header arguments.
    // Protect only their option/header prefix from the generic header matcher:
    // the value and every surrounding span stay visible to generic matching,
    // so a distinct Cookie header on the same physical line cannot be skipped.
    for option_prefix in SHELL_COOKIE_HEADER_OPTION_RE.find_iter(input) {
        output.push_str(&replace_header_value(
            &input[cursor..option_prefix.start()],
            &COOKIE_HEADER_RE,
            marker,
        ));
        output.push_str(option_prefix.as_str());
        cursor = option_prefix.end();
    }
    output.push_str(&replace_header_value(
        &input[cursor..],
        &COOKIE_HEADER_RE,
        marker,
    ));
    output
}

/// Redact sensitive HTTP header values while preserving useful header context.
///
/// Authorization keeps a recognized authentication scheme (for example,
/// `Bearer`) so diagnostics remain actionable. Cookie and Set-Cookie values are
/// always masked in full because their first token is secret material, not a
/// scheme. `marker` lets each presentation surface retain its existing
/// redaction vocabulary without duplicating the parsing rules.
pub(crate) fn redact_sensitive_headers(input: &str, marker: &str) -> String {
    let redacted = replace_header_value(input, &AUTH_HEADER_RE, marker);
    redact_cookie_headers(&redacted, marker)
}

/// Redact Cookie and Set-Cookie values without applying Authorization parsing.
///
/// Compact command renderers use their own token-aware Authorization display
/// rules, but still share this stricter whole-cookie contract.
pub(crate) fn redact_cookie_headers(input: &str, marker: &str) -> String {
    let redacted = replace_quoted_header_value(input, &ANSI_C_QUOTED_COOKIE_HEADER_RE, marker);
    let redacted = replace_quoted_header_value(&redacted, &SINGLE_QUOTED_COOKIE_HEADER_RE, marker);
    let redacted = replace_quoted_header_value(&redacted, &DOUBLE_QUOTED_COOKIE_HEADER_RE, marker);
    let redacted = replace_header_value(&redacted, &COMMAND_UNQUOTED_COOKIE_HEADER_RE, marker);
    let redacted = redact_curl_cookie_options(&redacted, marker);
    replace_generic_cookie_headers(&redacted, marker)
}

pub(crate) fn contains_sensitive_header_prefix(input: &str) -> bool {
    SENSITIVE_HEADER_PREFIX_RE.is_match(input)
        || (CURL_COMMAND_RE.is_match(input) && CURL_COOKIE_OPTION_PREFIX_RE.is_match(input))
}

fn is_cookie_header_name(name: &str) -> bool {
    let name = name.trim();
    name.eq_ignore_ascii_case("cookie") || name.eq_ignore_ascii_case("set-cookie")
}

/// Serialize a JSON value compactly after redacting Cookie-bearing strings.
///
/// Redacting before serialization avoids JSON quote/escape boundaries blocking
/// the text matcher. Cookie/Set-Cookie map values and contextual array values
/// (for example `["Cookie", "session=secret"]`) are opaque credentials even
/// when the value itself lacks a header prefix. Writing directly to the output
/// also avoids cloning the complete Value, and sanitizes object keys without
/// key-collision loss.
pub(crate) fn serialize_json_with_redacted_cookie_headers(
    value: &serde_json::Value,
    marker: &str,
) -> String {
    fn push_redacted_cookie_value(output: &mut String, value: &serde_json::Value, marker: &str) {
        match value {
            // Header maps commonly represent repeated Set-Cookie values as an
            // array. Preserve its arity for useful diagnostics while treating
            // every element as credential material. Objects remain opaque as a
            // whole because their keys can themselves contain cookie data.
            serde_json::Value::Array(values) => {
                output.push('[');
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        output.push(',');
                    }
                    push_redacted_cookie_value(output, value, marker);
                }
                output.push(']');
            }
            _ => output.push_str(
                &serde_json::to_string(marker).expect("serializing a JSON marker cannot fail"),
            ),
        }
    }

    fn push_value(output: &mut String, value: &serde_json::Value, marker: &str) {
        match value {
            serde_json::Value::Null => output.push_str("null"),
            serde_json::Value::Bool(value) => {
                output.push_str(if *value { "true" } else { "false" })
            }
            serde_json::Value::Number(value) => output.push_str(&value.to_string()),
            serde_json::Value::String(value) => {
                let redacted = redact_cookie_headers(value, marker);
                output.push_str(
                    &serde_json::to_string(&redacted)
                        .expect("serializing a JSON string cannot fail"),
                );
            }
            serde_json::Value::Array(values) => {
                output.push('[');
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        output.push(',');
                    }
                    let follows_cookie_header_name = index
                        .checked_sub(1)
                        .and_then(|previous| values[previous].as_str())
                        .is_some_and(is_cookie_header_name);
                    if follows_cookie_header_name {
                        push_redacted_cookie_value(output, value, marker);
                    } else {
                        push_value(output, value, marker);
                    }
                }
                output.push(']');
            }
            serde_json::Value::Object(values) => {
                output.push('{');
                for (index, (key, value)) in values.iter().enumerate() {
                    if index > 0 {
                        output.push(',');
                    }
                    let redacted_key = redact_cookie_headers(key, marker);
                    output.push_str(
                        &serde_json::to_string(&redacted_key)
                            .expect("serializing a JSON object key cannot fail"),
                    );
                    output.push(':');
                    if is_cookie_header_name(key) {
                        push_redacted_cookie_value(output, value, marker);
                    } else {
                        push_value(output, value, marker);
                    }
                }
                output.push('}');
            }
        }
    }

    let mut output = String::new();
    push_value(&mut output, value, marker);
    output
}

pub(crate) fn redact_known_secrets(input: &str) -> String {
    // Mask whole PEM private-key blocks first so later single-token rules cannot
    // leave the key body behind, and so the registered-secret pass is unaffected.
    let redacted = PRIVATE_KEY_BLOCK_RE.replace_all(input, "***");
    let redacted = POSTGRES_DSN_RE.replace_all(&redacted, |captures: &regex::Captures<'_>| {
        mask_dsn_password(captures.get(0).map(|m| m.as_str()).unwrap_or_default())
    });
    let redacted = redact_sensitive_headers(&redacted, "***");
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
        contains_sensitive_header_prefix, dsn_password, mask_dsn_password, redact_known_secrets,
        redact_sensitive_headers, register_known_secret, register_secret_or_dsn,
        serialize_json_with_redacted_cookie_headers,
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
    fn cookie_headers_mask_the_entire_value_without_treating_a_token_as_a_scheme() {
        let redacted = redact_known_secrets(
            "Cookie: session=abc; theme=dark\n\
             set-cookie\t:\taccess=xyz; HttpOnly; Secure\n\
             COOKIE: sessionSecret trailing-data\n\
             visible next line",
        );

        assert!(redacted.contains("Cookie: ***"));
        assert!(redacted.contains("set-cookie\t:\t***"));
        assert!(redacted.contains("COOKIE: ***"));
        assert!(redacted.contains("visible next line"));
        for secret in [
            "session=abc",
            "theme=dark",
            "access=xyz",
            "sessionSecret",
            "trailing-data",
        ] {
            assert!(
                !redacted.contains(secret),
                "cookie leak ({secret}): {redacted}"
            );
        }
    }

    #[test]
    fn folded_cookie_headers_are_fully_masked_without_consuming_the_next_header() {
        let redacted = redact_known_secrets(
            "Cookie: first=secret\r\n second=secret-two\nX-Trace: visible\n\
             Set-Cookie:\r\n session=empty-first-line\nContent-Type: text/plain",
        );

        assert!(redacted.contains("Cookie: ***\nX-Trace: visible"));
        assert!(redacted.contains("Set-Cookie:***\nContent-Type: text/plain"));
        assert!(!redacted.contains("first=secret"));
        assert!(!redacted.contains("second=secret-two"));
        assert!(!redacted.contains("session=empty-first-line"));
    }

    #[test]
    fn sensitive_header_redaction_supports_surface_specific_markers() {
        let redacted = redact_sensitive_headers(
            "Authorization: Bearer auth-secret\nCookie: cookie-secret",
            "[REDACTED]",
        );

        assert_eq!(
            redacted,
            "Authorization: Bearer [REDACTED]\nCookie: [REDACTED]"
        );
    }

    #[test]
    fn cookie_header_names_do_not_match_distinct_header_suffixes() {
        let redacted = redact_known_secrets(
            "X-Cookie: visible-one\nCookieJar: visible-two\nX-Set-Cookie: visible-three",
        );

        assert_eq!(
            redacted,
            "X-Cookie: visible-one\nCookieJar: visible-two\nX-Set-Cookie: visible-three"
        );
    }

    #[test]
    fn attached_curl_header_option_redacts_cookie_without_matching_distinct_names() {
        let redacted = redact_sensitive_headers(
            "curl -HCookie:session=secret -HSet-Cookie:access=secret-two -HX-Cookie:visible-one -HCookieJar:visible-two",
            "***",
        );

        assert!(redacted.contains("-HCookie:***"));
        assert!(redacted.contains("-HSet-Cookie:***"));
        assert!(redacted.contains("-HX-Cookie:visible-one"));
        assert!(redacted.contains("-HCookieJar:visible-two"));
        assert!(!redacted.contains("session=secret"));
        assert!(!redacted.contains("access=secret-two"));
    }

    #[test]
    fn quoted_cookie_header_arguments_preserve_delimiters_and_following_command() {
        let redacted = redact_sensitive_headers(
            "curl -H 'Cookie: session=secret one' -H\"Set-Cookie: access=secret-two\" https://example.test && echo done; printf 'X-Cookie: visible'",
            "***",
        );

        assert!(redacted.contains("-H 'Cookie: ***'"));
        assert!(redacted.contains("-H\"Set-Cookie: ***\""));
        assert!(redacted.contains("https://example.test && echo done"));
        assert!(redacted.contains("'X-Cookie: visible'"));
        assert!(!redacted.contains("session=secret"));
        assert!(!redacted.contains("access=secret-two"));
    }

    #[test]
    fn adjacent_shell_word_fragments_are_consumed_with_quoted_cookie_values() {
        let redacted = redact_sensitive_headers(
            r#"curl -H 'Cookie: 'session=adjacent-one https://one.test
curl --header="Set-Cookie: "access=adjacent-two https://two.test
curl -H $'Cookie: 'session=adjacent-three https://three.test
curl --cookie 'session='adjacent-four https://four.test"#,
            "***",
        );

        assert!(redacted.contains("-H 'Cookie: ***'"));
        assert!(redacted.contains("--header=\"Set-Cookie: ***\""));
        assert!(redacted.contains("-H $'Cookie: ***'"));
        assert!(redacted.contains("--cookie '***'"));
        for url in [
            "https://one.test",
            "https://two.test",
            "https://three.test",
            "https://four.test",
        ] {
            assert!(redacted.contains(url), "missing URL ({url}): {redacted}");
        }
        assert!(!redacted.contains("adjacent-"), "got: {redacted}");
    }

    #[test]
    fn ansi_c_quoted_cookie_headers_preserve_delimiters_and_following_command() {
        let redacted = redact_sensitive_headers(
            r#"curl -H $'Cookie: session=secret\'tail=secret-two' -H$'Set-Cookie: access=secret-three' -H $'X-Cookie: visible' https://example.test && echo done"#,
            "***",
        );

        assert!(redacted.contains("-H $'Cookie: ***'"));
        assert!(redacted.contains("-H$'Set-Cookie: ***'"));
        assert!(redacted.contains("-H $'X-Cookie: visible'"));
        assert!(redacted.contains("https://example.test && echo done"));
        assert!(!redacted.contains("session=secret"));
        assert!(!redacted.contains("tail=secret-two"));
        assert!(!redacted.contains("access=secret-three"));
    }

    #[test]
    fn curl_header_option_spellings_share_whole_cookie_redaction() {
        let redacted = redact_sensitive_headers(
            r#"curl -sHCookie:cluster-secret --header 'Set-Cookie: long-secret' --header=$'Cookie: ansi-secret\'tail-secret' --header="Cookie: double-secret" --header=Cookie:equals-secret -HX-Cookie:visible https://example.test"#,
            "***",
        );

        assert!(redacted.contains("-sHCookie:***"));
        assert!(redacted.contains("--header 'Set-Cookie: ***'"));
        assert!(redacted.contains("--header=$'Cookie: ***'"));
        assert!(redacted.contains("--header=\"Cookie: ***\""));
        assert!(redacted.contains("--header=Cookie:***"));
        assert!(
            redacted.contains("-HX-Cookie:visible"),
            "distinct header was changed: {redacted}"
        );
        assert!(redacted.contains("https://example.test"));
        for secret in [
            "cluster-secret",
            "long-secret",
            "ansi-secret",
            "tail-secret",
            "double-secret",
            "equals-secret",
        ] {
            assert!(
                !redacted.contains(secret),
                "cookie leak ({secret}): {redacted}"
            );
        }
    }

    #[test]
    fn curl_cookie_data_options_are_redacted_without_matching_unrelated_flags() {
        let redacted = redact_sensitive_headers(
            "curl --cookie session=one -b \"access=two\" -sb$'third=three\\'tail=four' --cookie='fifth=five' --cookie=sixth=six --cookie-jar visible.jar -c visible-two.jar https://example.test\nother -b visible-non-curl",
            "***",
        );

        assert!(redacted.contains("--cookie ***"));
        assert!(redacted.contains("-b \"***\""));
        assert!(
            redacted.contains("-sb$'***'"),
            "ANSI-C delimiter was changed: {redacted}"
        );
        assert!(redacted.contains("--cookie='***'"));
        assert!(redacted.contains("--cookie=***"));
        assert!(redacted.contains("--cookie-jar visible.jar"));
        assert!(redacted.contains("-c visible-two.jar"));
        assert!(redacted.contains("other -b visible-non-curl"));
        assert!(redacted.contains("https://example.test"));
        for secret in [
            "session=one",
            "access=two",
            "third=three",
            "tail=four",
            "fifth=five",
            "sixth=six",
        ] {
            assert!(
                !redacted.contains(secret),
                "cookie leak ({secret}): {redacted}"
            );
        }
    }

    #[test]
    fn curl_cookie_data_options_follow_shell_line_continuations() {
        let redacted = redact_sensitive_headers(
            "curl --cookie \\\n session=continued-one https://one.test\ncurl -b \\\r\n \"access=continued-two\" https://two.test",
            "***",
        );

        assert!(redacted.contains("--cookie \\\n ***"), "got: {redacted:?}");
        assert!(redacted.contains("-b \\\r\n \"***\""), "got: {redacted:?}");
        assert!(redacted.contains("https://one.test"), "got: {redacted:?}");
        assert!(redacted.contains("https://two.test"), "got: {redacted:?}");
        assert!(!redacted.contains("continued-one"), "got: {redacted:?}");
        assert!(!redacted.contains("continued-two"), "got: {redacted:?}");
    }

    #[test]
    fn mixed_generic_and_curl_cookie_headers_are_all_redacted() {
        let redacted = redact_sensitive_headers(
            "Cookie: first=secret && curl -H 'Cookie: second=secret' https://one.test\ncurl -H Cookie:third=secret https://two.test && Cookie: fourth=secret",
            "***",
        );

        assert!(redacted.contains("-H 'Cookie: ***'"));
        assert!(redacted.contains("-H Cookie:***"));
        assert!(redacted.contains("https://one.test"));
        assert!(redacted.contains("https://two.test"));
        for secret in [
            "first=secret",
            "second=secret",
            "third=secret",
            "fourth=secret",
        ] {
            assert!(
                !redacted.contains(secret),
                "mixed Cookie leak ({secret}): {redacted}"
            );
        }
    }

    #[test]
    fn shell_escaped_curl_header_arguments_redact_the_complete_word() {
        let redacted = redact_sensitive_headers(
            r"curl -H Cookie:\ session=escaped-one --header=Set-Cookie:access\ token=escaped-two https://example.test",
            "***",
        );

        assert!(redacted.contains("-H Cookie:***"));
        assert!(redacted.contains("--header=Set-Cookie:***"));
        assert!(redacted.contains("https://example.test"));
        assert!(!redacted.contains("escaped-one"));
        assert!(!redacted.contains("escaped-two"));
    }

    #[test]
    fn unquoted_cookie_header_values_accept_continuations_and_quoted_first_fragments() {
        let redacted = redact_sensitive_headers(
            "curl -H Cookie:\\\nsession=continued-one https://one.test\n\
             curl -H Cookie:'session=quoted-two' https://two.test\n\
             wget --header=Set-Cookie:\\\r\n\"access=continued-three\" https://three.test",
            "***",
        );

        assert!(redacted.contains("-H Cookie:***"), "got: {redacted:?}");
        assert!(
            redacted.contains("--header=Set-Cookie:***"),
            "got: {redacted:?}"
        );
        for url in ["https://one.test", "https://two.test", "https://three.test"] {
            assert!(redacted.contains(url), "missing URL ({url}): {redacted:?}");
        }
        for secret in ["continued-one", "quoted-two", "continued-three"] {
            assert!(
                !redacted.contains(secret),
                "continued Cookie leak ({secret}): {redacted:?}"
            );
        }
    }

    #[test]
    fn multiline_quoted_cookie_header_arguments_are_redacted_as_one_shell_word() {
        let redacted = redact_sensitive_headers(
            "curl -H 'Cookie: session=multiline-one\ncontinued=multiline-two' https://one.test\n\
             curl --header=\"Set-Cookie: access=multiline-three\ncontinued=multiline-four\" https://two.test\n\
             curl -H $'Cookie: session=multiline-five\ncontinued=multiline-six' https://three.test",
            "***",
        );

        assert!(redacted.contains("-H 'Cookie: ***'"), "got: {redacted:?}");
        assert!(
            redacted.contains("--header=\"Set-Cookie: ***\""),
            "got: {redacted:?}"
        );
        assert!(redacted.contains("-H $'Cookie: ***'"), "got: {redacted:?}");
        for url in ["https://one.test", "https://two.test", "https://three.test"] {
            assert!(redacted.contains(url), "missing URL ({url}): {redacted:?}");
        }
        assert!(!redacted.contains("multiline-"), "got: {redacted:?}");
    }

    #[test]
    fn wget_header_equals_arguments_share_cookie_redaction() {
        let redacted = redact_sensitive_headers(
            "wget --header=Cookie:session=wget-one --header='Set-Cookie: access=wget-two' https://example.test",
            "***",
        );

        assert!(redacted.contains("--header=Cookie:***"));
        assert!(redacted.contains("--header='Set-Cookie: ***'"));
        assert!(redacted.contains("https://example.test"));
        assert!(!redacted.contains("wget-one"));
        assert!(!redacted.contains("wget-two"));
    }

    #[test]
    fn compact_json_serialization_redacts_nested_cookie_strings_and_keys() {
        let value = serde_json::json!({
            "headers": "Cookie: json-secret",
            "nested": ["curl --cookie option=json-secret-two https://example.test"],
            "Cookie: key-secret": "Set-Cookie: value-secret",
            "safe": "X-Cookie: visible"
        });

        let serialized = serialize_json_with_redacted_cookie_headers(&value, "***");
        let reparsed: serde_json::Value = serde_json::from_str(&serialized).unwrap();

        assert!(serialized.contains("Cookie: ***"));
        assert!(serialized.contains("--cookie ***"));
        assert!(serialized.contains("X-Cookie: visible"));
        assert!(reparsed.is_object());
        for secret in [
            "json-secret",
            "json-secret-two",
            "key-secret",
            "value-secret",
        ] {
            assert!(
                !serialized.contains(secret),
                "JSON cookie leak ({secret}): {serialized}"
            );
        }
    }

    #[test]
    fn compact_json_serialization_redacts_values_owned_by_cookie_header_keys() {
        let value = serde_json::json!({
            "headers": {
                "Cookie": "session=json-map-one",
                "Set-Cookie": ["access=json-map-two"],
                "cookie": {"nested": "json-map-three"},
                "X-Cookie": "visible"
            }
        });

        let serialized = serialize_json_with_redacted_cookie_headers(&value, "***");
        let reparsed: serde_json::Value = serde_json::from_str(&serialized).unwrap();

        assert_eq!(reparsed["headers"]["Cookie"], "***");
        assert_eq!(
            reparsed["headers"]["Set-Cookie"],
            serde_json::json!(["***"])
        );
        assert_eq!(reparsed["headers"]["cookie"], "***");
        assert_eq!(reparsed["headers"]["X-Cookie"], "visible");
        assert!(!serialized.contains("json-map-"), "got: {serialized}");
    }

    #[test]
    fn compact_json_serialization_redacts_cookie_values_in_contextual_arrays() {
        let value = serde_json::json!({
            "headers": [
                ["Cookie", "session=json-pair-one"],
                ["set-cookie", ["access=json-pair-two", "refresh=json-pair-three"]],
                ["X-Cookie", "visible"]
            ],
            "argv_style": ["Cookie", "session=json-argv-four", "safe"]
        });

        let serialized = serialize_json_with_redacted_cookie_headers(&value, "***");
        let reparsed: serde_json::Value = serde_json::from_str(&serialized).unwrap();

        assert_eq!(reparsed["headers"][0], serde_json::json!(["Cookie", "***"]));
        assert_eq!(
            reparsed["headers"][1],
            serde_json::json!(["set-cookie", ["***", "***"]])
        );
        assert_eq!(
            reparsed["headers"][2],
            serde_json::json!(["X-Cookie", "visible"])
        );
        assert_eq!(
            reparsed["argv_style"],
            serde_json::json!(["Cookie", "***", "safe"])
        );
        assert!(!serialized.contains("json-pair-"), "got: {serialized}");
        assert!(!serialized.contains("json-argv-"), "got: {serialized}");
    }

    #[test]
    fn sensitive_header_prefix_detector_distinguishes_suffix_names() {
        assert!(contains_sensitive_header_prefix("Cookie: partial"));
        assert!(contains_sensitive_header_prefix("curl -H'Cookie: partial"));
        assert!(contains_sensitive_header_prefix("curl -sHCookie: partial"));
        assert!(contains_sensitive_header_prefix(
            "curl --header=$'Cookie: partial"
        ));
        assert!(contains_sensitive_header_prefix(
            "curl --cookie session=partial"
        ));
        assert!(contains_sensitive_header_prefix(
            "Authorization: Bearer partial"
        ));
        assert!(!contains_sensitive_header_prefix("X-Cookie: visible"));
        assert!(!contains_sensitive_header_prefix("CookieJar: visible"));
        assert!(!contains_sensitive_header_prefix("other -b visible"));
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
