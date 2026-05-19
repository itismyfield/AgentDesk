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

/// `GET /api/github/pr-summary` — fetch a PR summary from cache (or
/// fallback to `gh pr view`).
pub async fn get_pr_summary(
    State(_state): State<AppState>,
    Query(query): Query<PrSummaryQuery>,
) -> (StatusCode, Json<Value>) {
    if let Err(msg) = validate_repo(&query.repo) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": msg, "field": "repo" })),
        );
    }
    if query.pr <= 0 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "pr must be a positive integer", "field": "pr" })),
        );
    }

    let cache: &PrSummaryCache = pr_summary::shared();
    let opts = FetchOptions {
        force_refresh: query.force_refresh,
        expected_head_sha: query.expected_head_sha.clone(),
    };

    let repo = query.repo.clone();
    let pr = query.pr;
    // `gh` is a blocking child process — keep it off the runtime thread.
    let outcome = tokio::task::spawn_blocking(move || cache.fetch(&repo, pr, &opts))
        .await
        .map_err(|join_err| format!("pr_summary fetch task join: {join_err}"));

    match outcome {
        Ok(Ok(summary)) => (
            StatusCode::OK,
            Json(json!({
                "repo": summary.repo,
                "pr": summary.pr_number,
                "cache_hit": summary.cache_hit,
                "age_seconds": summary.age_seconds,
                "head_sha": summary.head_sha,
                "view": summary.view,
            })),
        ),
        Ok(Err(err)) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": err, "source": "gh" })),
        ),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": err })),
        ),
    }
}

/// `POST /api/github/pr-summary/invalidate` — drop a single cached PR. Used
/// by GitHub webhook handlers (or operators) to force a refresh on the next
/// lookup. Idempotent — invalidating a PR that isn't cached is a no-op.
pub async fn invalidate_pr_summary(
    State(_state): State<AppState>,
    Json(body): Json<PrInvalidateBody>,
) -> (StatusCode, Json<Value>) {
    if let Err(msg) = validate_repo(&body.repo) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": msg, "field": "repo" })),
        );
    }
    if body.pr <= 0 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "pr must be a positive integer", "field": "pr" })),
        );
    }
    pr_summary::shared().invalidate(&body.repo, body.pr);
    (
        StatusCode::OK,
        Json(json!({ "status": "ok", "repo": body.repo, "pr": body.pr })),
    )
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
}
