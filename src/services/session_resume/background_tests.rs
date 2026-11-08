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
    executable(&path, &script);
    crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_CLAUDE_PATH",
        &path,
    )
}

/// An idle row bound to `bound` at `cwd`, and a transcript for `id` there so only the background
/// check can refuse it.
async fn seeded(pool: &sqlx::PgPool, (home, cwd): (&Path, &Path), bound: &str, id: &str) {
    sqlx::query("INSERT INTO sessions (session_key, provider, status, cwd, claude_session_id) VALUES ($1, 'claude', 'idle', $2, $3) ON CONFLICT (session_key) DO UPDATE SET cwd=$2, claude_session_id=$3")
        .bind(KEY).bind(cwd.to_str().unwrap()).bind(bound).execute(pool).await.unwrap();
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

/// A shell `echo` of an `agents --json` list in the CLI's shape, one session per `(kind, id)`.
fn listing(sessions: &[(&str, &str)]) -> String {
    let entries = sessions.iter().map(|(kind, id)| {
        format!(r#"{{"pid":1,"cwd":"/","kind":"{kind}","startedAt":0,"sessionId":"{id}","name":"n","status":"idle"}}"#)
    });
    format!("echo '[{}]'", entries.collect::<Vec<_>>().join(","))
}

fn executable(path: &Path, script: &str) {
    std::fs::write(path, script).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
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
    let _claude = fake_claude(bin.path(), &listing(&[("background", &id)]));
    block_on(async {
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        seeded(&pool, (home.path(), cwd.path()), "old", &id).await;
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

// A list naming other sessions, a list in another shape, a missing CLI and a CLI that never
// answers all leave the /resume to rebind as before.
#[test]
fn an_unlisted_or_unreadable_background_list_still_rebinds_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (bin, home, cwd) = (
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    );
    let other = uuid::Uuid::new_v4().to_string();
    let cases = [
        ("other session listed", listing(&[("background", &other)])),
        ("not an array", r#"echo '{"sessions":[]}'"#.to_string()),
        (
            "entry without kind",
            r#"echo '[{"sessionId":"x"}]'"#.to_string(),
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
            seeded(&pool, (home.path(), cwd.path()), "old", &id).await;
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

// The channel's own idle foreground session U listed as `interactive` resumes as before; the same
// U listed as `background` is refused and the binding stays.
#[test]
fn a_foreground_session_resumes_and_the_same_session_in_the_background_is_refused_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (bin, home, cwd) = (
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    );
    let id = uuid::Uuid::new_v4().to_string();
    block_on(async {
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        seeded(&pool, (home.path(), cwd.path()), &id, &id).await;
        let foreground = fake_claude(bin.path(), &listing(&[("interactive", &id)]));
        let resumed = resume(&pool, home.path(), cwd.path(), &id).await;
        assert_eq!(resumed.map_err(|e| format!("{e:?}")), Ok(id.clone()));
        drop(foreground);
        let _background = fake_claude(bin.path(), &listing(&[("background", &id)]));
        let refused = resume(&pool, home.path(), cwd.path(), &id).await;
        let (status, _) = refused.expect_err("a background session").into_response();
        assert_eq!(status, axum::http::StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(binding(&pool).await, id);
        pool.close().await;
        db.drop().await;
    });
}

// A resolver whose version probes hang is cut at the lookup deadline with the CLI call, and the
// resume still rebinds; only PATH and a login shell printing that PATH offer candidates.
#[test]
fn a_hanging_resolver_is_cut_at_the_lookup_deadline_and_the_resume_rebinds_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (bins, home, cwd) = (
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    );
    // Several candidates make the resolver probe each one's `--version`, which never answers here.
    let mut dirs = Vec::new();
    for n in 0..3 {
        let dir = bins.path().join(format!("bin{n}"));
        std::fs::create_dir_all(&dir).unwrap();
        let script = "#!/bin/sh\n[ \"$1\" = --version ] && exec tail -f /dev/null\necho '[]'\n";
        executable(&dir.join("claude"), script);
        dirs.push(dir.display().to_string());
    }
    let shell = bins.path().join("login-shell");
    executable(&shell, "#!/bin/sh\nexec /bin/sh -c \"$2\"\n");
    let path = format!("{}:/usr/bin:/bin", dirs.join(":"));
    let set = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock;
    let _path = set("PATH", std::ffi::OsStr::new(&path));
    let _home = set("HOME", home.path().as_os_str());
    let _shell = set("SHELL", shell.as_os_str());
    let _no_override =
        crate::config::TestEnvVarGuard::remove_after_shared_test_env_lock("AGENTDESK_CLAUDE_PATH");
    let id = uuid::Uuid::new_v4().to_string();
    block_on(async {
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        seeded(&pool, (home.path(), cwd.path()), "old", &id).await;
        let started = std::time::Instant::now();
        let rebound = resume(&pool, home.path(), cwd.path(), &id).await;
        let waited = started.elapsed();
        assert_eq!(rebound.map_err(|e| format!("{e:?}")), Ok(id.clone()));
        // Three probes at their 2s cap take 6s, so finishing well under that shows the deadline.
        assert!(waited < std::time::Duration::from_secs(5), "{waited:?}");
        pool.close().await;
        db.drop().await;
    });
}

// A channel whose next launch settles on a named account home is looked up under that home, so a
// background session listed only there is refused.
#[test]
fn the_lookup_reads_the_named_home_the_next_launch_uses_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (bin, home, cwd, user) = (
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    );
    let user = std::fs::canonicalize(user.path()).unwrap();
    let (default_home, named) = (
        user.join(".claude"),
        user.join(".adk/profiles/claude/acct-b"),
    );
    let id = uuid::Uuid::new_v4().to_string();
    std::fs::create_dir_all(&default_home).unwrap();
    std::fs::create_dir_all(&named).unwrap();
    std::fs::write(default_home.join("agents.json"), "[]").unwrap();
    let background = listing(&[("background", &id)]).replace("echo ", "");
    let background = background.trim_matches('\'');
    std::fs::write(named.join("agents.json"), background).unwrap();
    let org = crate::services::discord::runtime_store::org_schema_path_for_root(
        &crate::config::runtime_root().unwrap(),
    );
    std::fs::create_dir_all(org.parent().unwrap()).unwrap();
    let schema = format!(
        "version: 1\nprovider_auth_profiles:\n  acct-b:\n    provider: claude\n    home: {}\nagents: {{}}\nprovider_auth_fallbacks:\n  claude:\n    enabled: false\nprovider_auth_primary_profiles:\n  claude: acct-b\n",
        named.display()
    );
    std::fs::write(&org, schema).unwrap();
    let set = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock;
    let _user = set("HOME", &user);
    let _default = set("CLAUDE_CONFIG_DIR", &default_home);
    let seen = bin.path().join("seen-home");
    let body = format!(
        "printf '%s' \"$CLAUDE_CONFIG_DIR\" > '{}'\ncat \"$CLAUDE_CONFIG_DIR/agents.json\"",
        seen.display()
    );
    let _claude = fake_claude(bin.path(), &body);
    block_on(async {
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        seeded(&pool, (home.path(), cwd.path()), "old", &id).await;
        let refused = resume(&pool, home.path(), cwd.path(), &id).await;
        let (status, _) = refused
            .expect_err("listed in the named home")
            .into_response();
        assert_eq!(status, axum::http::StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            std::fs::read_to_string(&seen).unwrap(),
            named.display().to_string()
        );
        assert_eq!(binding(&pool).await, "old");
        pool.close().await;
        db.drop().await;
    });
}
