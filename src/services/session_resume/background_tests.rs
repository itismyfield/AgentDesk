use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use super::{ResumePreviousOptions, ResumeRebindError, catalog, perform_resume_rebind};
use crate::services::provider::ProviderKind;

const KEY: &str = "i6483-background";

/// A fake `claude` running `body` for `agents --json`, which the resume resolves through the
/// override the shared env lock lets this test set.
fn fake_claude(dir: &Path, body: &str) -> crate::config::TestEnvVarGuard {
    let path = dir.join(format!("claude-{}", uuid::Uuid::new_v4().simple()));
    let script = format!("#!/bin/sh\n[ \"$1 $2\" = \"agents --json\" ] || exit 2\n{body}\n");
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_CLAUDE_PATH",
        &path,
    )
}

/// A row bound to `old` at `cwd`, and a transcript for `id` there so only the background check
/// can refuse it.
async fn seeded(pool: &sqlx::PgPool, home: &Path, cwd: &Path, id: &str) {
    sqlx::query("INSERT INTO sessions (session_key, provider, status, cwd, claude_session_id) VALUES ($1, 'claude', 'idle', $2, 'old') ON CONFLICT (session_key) DO UPDATE SET cwd=$2, claude_session_id='old'")
        .bind(KEY).bind(cwd.to_str().unwrap()).execute(pool).await.unwrap();
    let transcript =
        crate::services::claude_tui::transcript_tail::claude_transcript_path(cwd, id, Some(home))
            .unwrap();
    std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    std::fs::write(&transcript, "{}\n").unwrap();
}

async fn resume(
    pool: &sqlx::PgPool,
    home: &Path,
    cwd: &Path,
    id: &str,
) -> Result<String, ResumeRebindError> {
    let opts = ResumePreviousOptions {
        session_id: Some(id.to_string()),
        cwd: Some(cwd.to_string_lossy().into_owned()),
    };
    let claude = Some(ProviderKind::Claude);
    let rebind = perform_resume_rebind(pool, None, KEY, claude, None, "i6483-bg", &opts);
    let outcome = catalog::TEST_HOME.scope(home.to_path_buf(), rebind).await?;
    Ok(outcome.target_session_id)
}

async fn binding(pool: &sqlx::PgPool) -> String {
    sqlx::query_scalar("SELECT claude_session_id FROM sessions WHERE session_key=$1")
        .bind(KEY)
        .fetch_one(pool)
        .await
        .unwrap()
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    let mut builder = tokio::runtime::Builder::new_current_thread();
    builder.enable_all().build().unwrap().block_on(future)
}

// A /resume whose target `claude agents --json` lists is refused with the cleanup to do, and the
// channel keeps its binding.
#[test]
fn a_listed_background_session_is_refused_with_its_cleanup_and_keeps_the_binding_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (bin, home, cwd) = (
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    );
    let id = uuid::Uuid::new_v4().to_string();
    let listing = format!("echo '[{{\"sessionId\":\"{id}\",\"status\":\"backgrounded\"}}]'");
    let _claude = fake_claude(bin.path(), &listing);
    block_on(async {
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        seeded(&pool, home.path(), cwd.path(), &id).await;
        let refused = resume(&pool, home.path(), cwd.path(), &id).await;
        let (status, axum::Json(body)) = refused.expect_err("a background target").into_response();
        assert_eq!(status, axum::http::StatusCode::UNPROCESSABLE_ENTITY);
        let error = body["error"].as_str().unwrap();
        assert!(error.contains(&format!("세션 {}", &id[..8])), "{error}");
        assert!(error.contains("`claude agents`"), "{error}");
        assert_eq!(binding(&pool).await, "old");
        pool.close().await;
        db.drop().await;
    });
}

// A list naming other sessions, a missing CLI and a CLI that never answers all leave the /resume
// to rebind as before.
#[test]
fn an_unlisted_or_unreadable_background_list_still_rebinds_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (bin, home, cwd) = (
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    );
    let other = uuid::Uuid::new_v4();
    let cases = [
        (
            "other session listed",
            format!("echo '[{{\"sessionId\":\"{other}\"}}]'"),
        ),
        ("missing cli", String::new()),
        ("cli never answers", "exec tail -f /dev/null".to_string()),
    ];
    block_on(async {
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        for (case, body) in cases {
            let _claude = match body.is_empty() {
                true => crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
                    "AGENTDESK_CLAUDE_PATH",
                    &bin.path().join("absent-claude"),
                ),
                false => fake_claude(bin.path(), &body),
            };
            let id = uuid::Uuid::new_v4().to_string();
            seeded(&pool, home.path(), cwd.path(), &id).await;
            let rebound = resume(&pool, home.path(), cwd.path(), &id).await;
            assert_eq!(
                rebound.map_err(|e| format!("{e:?}")),
                Ok(id.clone()),
                "{case}"
            );
            assert_eq!(binding(&pool).await, id, "{case}");
        }
        pool.close().await;
        db.drop().await;
    });
}
