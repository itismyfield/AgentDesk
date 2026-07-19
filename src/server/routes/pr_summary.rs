//! HTTP endpoints for the PR summary cache (#2654).
//!
//! Agents call `GET /api/github/pr-summary?repo=…&pr=…` instead of shelling
//! out to `gh pr view` each turn. The cache layer is in
//! [`crate::services::pr_summary`].

use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::{Value, json};

use super::AppState;
use crate::error::{AppError, AppResult, ErrorCode};
use crate::services::pr_summary::{self, FetchOptions, PrSummaryCache};

#[derive(Debug, Deserialize)]
pub struct PrSummaryQuery {
    /// `owner/repo`. We accept arbitrary casing — the cache normalises.
    pub repo: String,
    /// PR number. Required.
    pub pr: i64,
    /// When set to `true`, bypass any cached value and refetch.
    #[serde(default)]
    pub force_refresh: bool,
    /// Optional caller-supplied head SHA. When present and non-empty, the
    /// cached entry is honored only when its `head_sha` matches.
    #[serde(default)]
    pub expected_head_sha: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PrInvalidateBody {
    pub repo: String,
    pub pr: i64,
}

fn validate_repo(repo: &str) -> Result<(), &'static str> {
    let trimmed = repo.trim();
    if trimmed.is_empty() {
        return Err("repo must not be empty");
    }
    // GitHub repo identifiers are `owner/name`. We disallow whitespace and
    // anything that does not match that shape to avoid passing junk to the
    // `gh` CLI.
    let mut parts = trimmed.split('/');
    let owner = parts.next().unwrap_or("");
    let name = parts.next().unwrap_or("");
    if owner.is_empty() || name.is_empty() || parts.next().is_some() {
        return Err("repo must be in 'owner/name' form");
    }
    let bad_char = |c: char| {
        c.is_whitespace()
            || c == '"'
            || c == '\''
            || c == ';'
            || c == '|'
            || c == '&'
            || c == '`'
            || c == '$'
    };
    if owner.chars().any(bad_char) || name.chars().any(bad_char) {
        return Err("repo contains invalid characters");
    }
    Ok(())
}

/// Validate the shared `repo` / `pr` inputs, surfacing failures as the
/// standard [`AppError`] (#4228). The HTTP status (400) and the human-readable
/// `error` message are preserved from the previous ad-hoc `json!` bodies; the
/// former top-level `field` hint is carried on the standard `context` object.
fn ensure_valid_query(repo: &str, pr: i64) -> AppResult<()> {
    if let Err(msg) = validate_repo(repo) {
        return Err(AppError::bad_request(msg).with_context("field", "repo"));
    }
    if pr <= 0 {
        return Err(
            AppError::bad_request("pr must be a positive integer").with_context("field", "pr")
        );
    }
    Ok(())
}

/// `GET /api/github/pr-summary` — fetch a PR summary from cache (or
/// fallback to `gh pr view`).
pub async fn get_pr_summary(
    State(_state): State<AppState>,
    Query(query): Query<PrSummaryQuery>,
) -> AppResult<(StatusCode, Json<Value>)> {
    ensure_valid_query(&query.repo, query.pr)?;

    let cache: &PrSummaryCache = pr_summary::shared();
    let opts = FetchOptions {
        force_refresh: query.force_refresh,
        expected_head_sha: query.expected_head_sha.clone(),
    };

    let repo = query.repo.clone();
    let pr = query.pr;
    // `gh` is a blocking child process — keep it off the runtime thread. A join
    // failure keeps its previous 500 + `{ "error": … }` contract via
    // `AppError::internal`.
    let summary = tokio::task::spawn_blocking(move || cache.fetch(&repo, pr, &opts))
        .await
        .map_err(|join_err| AppError::internal(format!("pr_summary fetch task join: {join_err}")))?
        // A gh CLI / cache miss failure keeps its 502; the former top-level
        // `source` hint moves onto the standard `context` object.
        .map_err(|err| {
            AppError::new(StatusCode::BAD_GATEWAY, ErrorCode::Dispatch, err)
                .with_context("source", "gh")
        })?;

    Ok((
        StatusCode::OK,
        Json(json!({
            "repo": summary.repo,
            "pr": summary.pr_number,
            "cache_hit": summary.cache_hit,
            "age_seconds": summary.age_seconds,
            "head_sha": summary.head_sha,
            "view": summary.view,
        })),
    ))
}

/// `POST /api/github/pr-summary/invalidate` — drop a single cached PR. Used
/// by GitHub webhook handlers (or operators) to force a refresh on the next
/// lookup. Idempotent — invalidating a PR that isn't cached is a no-op.
pub async fn invalidate_pr_summary(
    State(_state): State<AppState>,
    Json(body): Json<PrInvalidateBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    ensure_valid_query(&body.repo, body.pr)?;
    pr_summary::shared().invalidate(&body.repo, body.pr);
    Ok((
        StatusCode::OK,
        Json(json!({ "status": "ok", "repo": body.repo, "pr": body.pr })),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_repo_accepts_owner_name() {
        validate_repo("itismyfield/AgentDesk").unwrap();
        validate_repo("user/Repo-Name.dot_underscore").unwrap();
    }

    #[test]
    fn validate_repo_rejects_empty_pieces() {
        validate_repo("").unwrap_err();
        validate_repo("owner").unwrap_err();
        validate_repo("/repo").unwrap_err();
        validate_repo("owner/").unwrap_err();
        validate_repo("a/b/c").unwrap_err();
    }

    #[test]
    fn validate_repo_rejects_injection_chars() {
        for s in [
            "owner; rm -rf /",
            "ow ner/repo",
            "owner/repo\"",
            "owner/$(echo pwn)",
            "owner/`whoami`",
            "owner/repo|cat",
        ] {
            assert!(
                validate_repo(s).is_err(),
                "should reject suspicious repo string: {s:?}"
            );
        }
    }

    // #4228: the ad-hoc `json!` error bodies became `AppError`s. These lock the
    // externally observable contract — HTTP status and the `error` message —
    // and record that the former top-level `field` hint now rides on `context`.

    #[test]
    fn ensure_valid_query_accepts_good_input() {
        // Asserts a well-formed repo + positive PR yields no error, so the
        // happy path still falls through to the cache fetch unchanged.
        ensure_valid_query("itismyfield/AgentDesk", 42).unwrap();
    }

    #[test]
    fn ensure_valid_query_bad_repo_preserves_400_and_message() {
        // Asserts an invalid repo keeps HTTP 400 and the exact `validate_repo`
        // message under `error`, and that the `field=repo` hint is preserved
        // (relocated from the old top-level key to `context`).
        let err = ensure_valid_query("owner", 1).unwrap_err();
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        assert_eq!(err.message(), "repo must be in 'owner/name' form");
        assert_eq!(
            err.to_json_value()["error"],
            "repo must be in 'owner/name' form"
        );
        assert_eq!(
            err.context().get("field").and_then(Value::as_str),
            Some("repo")
        );
    }

    #[test]
    fn ensure_valid_query_nonpositive_pr_preserves_400_and_message() {
        // Asserts a non-positive PR keeps HTTP 400 and the exact "pr must be a
        // positive integer" message under `error`, with the `field=pr` hint
        // preserved on `context`.
        let err = ensure_valid_query("owner/repo", 0).unwrap_err();
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        assert_eq!(err.message(), "pr must be a positive integer");
        assert_eq!(
            err.to_json_value()["error"],
            "pr must be a positive integer"
        );
        assert_eq!(
            err.context().get("field").and_then(Value::as_str),
            Some("pr")
        );
    }

    #[test]
    fn gh_fetch_failure_maps_to_502_with_source_context() {
        // Asserts the gh CLI / cache-miss failure path keeps HTTP 502 (BAD_GATEWAY)
        // and surfaces the upstream string under `error`, with the former
        // top-level `source=gh` hint preserved on `context`. Mirrors the inline
        // `.map_err` in `get_pr_summary`.
        let err = AppError::new(StatusCode::BAD_GATEWAY, ErrorCode::Dispatch, "gh exploded")
            .with_context("source", "gh");
        assert_eq!(err.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(err.to_json_value()["error"], "gh exploded");
        assert_eq!(
            err.context().get("source").and_then(Value::as_str),
            Some("gh")
        );
    }
}
