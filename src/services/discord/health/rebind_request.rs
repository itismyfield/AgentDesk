use crate::services::discord::recovery_engine::ManualRebindOverrides;
use crate::services::provider::ProviderKind;

/// Parsed `/api/inflight/rebind` body, kept outside the recovery giant so the
/// operator-only validation surface can grow without re-inflating it.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct ParsedRebindRequest {
    pub(super) provider: ProviderKind,
    pub(super) channel_id: u64,
    pub(super) tmux_session: Option<String>,
    pub(super) overrides: ManualRebindOverrides,
}

pub(super) fn parse_rebind_body(body: &str) -> Result<ParsedRebindRequest, (&'static str, String)> {
    let json: serde_json::Value = serde_json::from_str(body).map_err(|_| {
        (
            "400 Bad Request",
            r#"{"ok":false,"error":"invalid JSON"}"#.to_string(),
        )
    })?;
    let provider_raw = json
        .get("provider")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .trim();
    let provider = ProviderKind::from_str(provider_raw).ok_or_else(|| {
        (
            "400 Bad Request",
            r#"{"ok":false,"error":"provider must be one of: claude, codex, gemini, opencode, qwen"}"#.to_string(),
        )
    })?;
    let channel_id = match json.get("channel_id") {
        Some(value) if value.is_u64() => value.as_u64().unwrap_or(0),
        Some(value) if value.is_string() => value
            .as_str()
            .unwrap_or("")
            .trim()
            .parse::<u64>()
            .unwrap_or(0),
        _ => 0,
    };
    if channel_id == 0 {
        return Err((
            "400 Bad Request",
            r#"{"ok":false,"error":"channel_id is required (non-zero u64)"}"#.to_string(),
        ));
    }
    let tmux_session = json
        .get("tmux_session")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let optional_override = |field: &'static str| -> Result<Option<&str>, (&'static str, String)> {
        match json.get(field) {
            None | Some(serde_json::Value::Null) => Ok(None),
            Some(value) => value.as_str().map(Some).ok_or_else(|| {
                (
                    "400 Bad Request",
                    serde_json::json!({
                        "ok": false,
                        "error": format!("{field} override must be a string")
                    })
                    .to_string(),
                )
            }),
        }
    };
    let overrides = ManualRebindOverrides::validated(
        &provider,
        optional_override("output_path")?,
        optional_override("session_id")?,
    )
    .map_err(|error| {
        (
            "400 Bad Request",
            serde_json::json!({"ok": false, "error": error}).to_string(),
        )
    })?;
    Ok(ParsedRebindRequest {
        provider,
        channel_id,
        tmux_session,
        overrides,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_rebind_request_rejects_non_string_override() {
        let (status, body) =
            parse_rebind_body(r#"{"provider":"codex","channel_id":"42","session_id":42}"#)
                .unwrap_err();
        assert_eq!(status, "400 Bad Request");
        assert!(body.contains("session_id override must be a string"));
    }

    #[test]
    fn health_rebind_request_threads_valid_session_override() {
        let request = parse_rebind_body(
            r#"{"provider":"codex","channel_id":"42","session_id":"4c474e5d-37e7-4b6a-bcf7-d68854a31c49"}"#,
        )
        .expect("valid request");
        assert_eq!(
            request.overrides.session_id(),
            Some("4c474e5d-37e7-4b6a-bcf7-d68854a31c49")
        );
    }
}
