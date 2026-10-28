use super::*;
use std::cell::RefCell;
use std::time::Duration;

use crate::services::tui_prompt_dedupe::TuiRuntimeBinding;

thread_local! {
    static METADATA_ERROR: RefCell<Option<(PathBuf, io::ErrorKind)>> = const { RefCell::new(None) };
    static CANONICAL_ERROR: RefCell<Option<(PathBuf, io::ErrorKind)>> = const { RefCell::new(None) };
}

pub(super) fn metadata_error(path: &Path) -> Option<io::ErrorKind> {
    METADATA_ERROR.with_borrow(|fault| {
        fault
            .as_ref()
            .filter(|(at, _)| at == path)
            .map(|(_, kind)| *kind)
    })
}

pub(super) fn canonical_error(path: &Path) -> Option<io::ErrorKind> {
    CANONICAL_ERROR.with_borrow(|fault| {
        fault
            .as_ref()
            .filter(|(at, _)| at == path)
            .map(|(_, kind)| *kind)
    })
}

struct Fault;

impl Drop for Fault {
    fn drop(&mut self) {
        METADATA_ERROR.with_borrow_mut(|fault| *fault = None);
        CANONICAL_ERROR.with_borrow_mut(|fault| *fault = None);
    }
}

struct Fixture {
    root: tempfile::TempDir,
    cwd: PathBuf,
    home: PathBuf,
    sid: String,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("current");
        let home = root.path().join("claude-home");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&home).unwrap();
        Self {
            root,
            cwd,
            home,
            sid: uuid::Uuid::new_v4().to_string(),
        }
    }

    fn request(&self) -> Request<'_> {
        Request {
            session_key: "claude/discord_0123456789abcdef/mac-book:AgentDesk-claude-plan",
            discord_token_hash: "discord_0123456789abcdef",
            logical_key: "AgentDesk-claude-plan",
            channel_id: 7_503,
            working_dir: &self.cwd,
            requested_session_id: None,
            force_fresh: false,
            claude_home: Some(&self.home),
        }
    }

    fn database(&self) -> ProviderSessionIds {
        ProviderSessionIds {
            resolved_session_key: self.request().session_key.into(),
            claude_session_id: Some(self.sid.clone()),
            raw_provider_session_id: Some(self.sid.clone()),
            cwd: Some(self.cwd.display().to_string()),
            cache_entry_age_secs: Some(3_600),
            raw_provider_transcript_len_watermark: None,
            raw_provider_transcript_watermark_session_id: None,
            raw_provider_transcript_growth_proven: false,
        }
    }

    fn path(&self, cwd: &Path, sid: &str) -> PathBuf {
        self.home
            .join("projects")
            .join(encode_project_path(cwd))
            .join(format!("{sid}.jsonl"))
    }

    fn transcript(&self, cwd: &Path, sid: &str) -> PathBuf {
        let path = self.path(cwd, sid);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            format!("{{\"sessionId\":\"{sid}\",\"type\":\"user\"}}\n"),
        )
        .unwrap();
        path
    }

    fn binding(&self, path: &Path) -> TuiRuntimeBinding {
        TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            session_id: Some(self.sid.clone()),
            output_path: path.display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            last_offset: 17,
            relay_last_offset: None,
        }
    }

    fn plan(
        &self,
        database: Result<Option<ProviderSessionIds>, String>,
        runtime: RuntimeObservation,
    ) -> Plan {
        evaluate(self.request(), database, runtime, Ok(None), Ok(Vec::new()))
    }
}

#[test]
fn a_canonical_selector_keeps_its_exact_source_and_plan_context() {
    let fx = Fixture::new();
    let path = fx.transcript(&fx.cwd, &fx.sid);
    let original = fs::read(&path).unwrap();
    let plan = fx.plan(Ok(Some(fx.database())), RuntimeObservation::Missing);
    let diagnostic = plan.diagnostic();
    let metadata = fs::metadata(&path).unwrap();
    let (dev, ino) = file_identity(&metadata);
    assert!(matches!(plan, Plan::Resume(..)));
    assert_eq!(diagnostic["decision"], "resume");
    assert_eq!(diagnostic["expected_native_session_id"], fx.sid);
    assert_eq!(diagnostic["context"]["expected_native_session_id"], fx.sid);
    assert_eq!(
        diagnostic["context"]["exact_source"]["path"],
        path.display().to_string()
    );
    assert_eq!(diagnostic["context"]["exact_source"]["dev"], dev);
    assert_eq!(diagnostic["context"]["exact_source"]["ino"], ino);
    assert_eq!(diagnostic["context"]["launch_mode"], "resume");
    assert_eq!(diagnostic["mode"], "plan_only");
    assert_eq!(
        fs::read(&path).unwrap(),
        original,
        "planning does not edit the source"
    );
}

#[test]
fn an_exact_binding_path_survives_a_different_current_cwd() {
    let fx = Fixture::new();
    let old = fx.root.path().join("old-worktree");
    fs::create_dir_all(&old).unwrap();
    let path = fx.transcript(&old, &fx.sid);
    let plan = fx.plan(
        Ok(Some(fx.database())),
        RuntimeObservation::Present(fx.binding(&path)),
    );
    assert!(matches!(&plan, Plan::Resume(source, ..) if source.path == path));
    assert_eq!(
        plan.diagnostic()["context"]["exact_source"]["path"],
        path.display().to_string()
    );
}

#[test]
fn a_database_error_is_a_hold_even_with_a_readable_binding() {
    let fx = Fixture::new();
    let path = fx.transcript(&fx.cwd, &fx.sid);
    let plan = fx.plan(
        Err("database unavailable".into()),
        RuntimeObservation::Present(fx.binding(&path)),
    );
    assert!(matches!(plan, Plan::Hold(HoldReason::Database(_), ..)));
}

#[test]
fn only_a_normal_absence_can_plan_a_fresh_session() {
    let fx = Fixture::new();
    assert!(matches!(
        fx.plan(Ok(None), RuntimeObservation::Missing),
        Plan::Fresh(FreshReason::SelectorAbsent, ..)
    ));
    assert!(matches!(
        fx.plan(Ok(Some(fx.database())), RuntimeObservation::Missing),
        Plan::Fresh(FreshReason::TranscriptAbsent, ..)
    ));
    let empty = ProviderSessionIds {
        claude_session_id: None,
        raw_provider_session_id: None,
        ..fx.database()
    };
    assert!(matches!(
        fx.plan(Ok(Some(empty)), RuntimeObservation::Missing),
        Plan::Fresh(FreshReason::SelectorAbsent, ..)
    ));
}

#[test]
fn explicit_fresh_does_not_restore_old_selectors_or_depend_on_database_health() {
    let fx = Fixture::new();
    let mut request = fx.request();
    request.force_fresh = true;
    request.requested_session_id = Some(&fx.sid);
    let plan = evaluate(
        request,
        Err("offline".into()),
        RuntimeObservation::Missing,
        Err("unreadable".into()),
        Err("corrupt".into()),
    );
    assert!(matches!(&plan, Plan::Fresh(FreshReason::Explicit, ..)));
    assert!(plan.diagnostic()["context"]["expected_native_session_id"].is_null());
}

#[test]
fn a_requested_selector_must_agree_with_the_canonical_chooser() {
    let fx = Fixture::new();
    fx.transcript(&fx.cwd, &fx.sid);
    let requested = uuid::Uuid::new_v4().to_string();
    let mut request = fx.request();
    request.requested_session_id = Some(&requested);
    let plan = evaluate(
        request,
        Ok(Some(fx.database())),
        RuntimeObservation::Missing,
        Ok(None),
        Ok(Vec::new()),
    );
    assert!(matches!(plan, Plan::Hold(HoldReason::Conflict(_), ..)));
}

#[test]
fn invalid_or_mismatched_runtime_selectors_never_become_absence() {
    let fx = Fixture::new();
    let path = fx.transcript(&fx.cwd, &fx.sid);
    for id in [
        "not-a-native-uuid".to_string(),
        uuid::Uuid::new_v4().to_string(),
    ] {
        let mut binding = fx.binding(&path);
        binding.session_id = Some(id);
        assert!(matches!(
            fx.plan(
                Ok(Some(fx.database())),
                RuntimeObservation::Present(binding)
            ),
            Plan::Hold(..)
        ));
    }
    let mut binding = fx.binding(&path);
    binding.runtime_kind = RuntimeHandoffKind::CodexTui;
    assert!(matches!(
        fx.plan(
            Ok(Some(fx.database())),
            RuntimeObservation::Present(binding)
        ),
        Plan::Hold(HoldReason::Runtime(_), ..)
    ));
}

#[test]
fn a_missing_canonical_row_with_runtime_evidence_is_a_hold() {
    let fx = Fixture::new();
    let path = fx.transcript(&fx.cwd, &fx.sid);
    assert!(matches!(
        fx.plan(Ok(None), RuntimeObservation::Present(fx.binding(&path))),
        Plan::Hold(HoldReason::Conflict(_), ..)
    ));
}

#[test]
fn metadata_errors_cannot_turn_a_known_selector_into_fresh() {
    let fx = Fixture::new();
    let path = fx.path(&fx.cwd, &fx.sid);
    for kind in [io::ErrorKind::PermissionDenied, io::ErrorKind::Other] {
        let _fault = Fault;
        METADATA_ERROR.with_borrow_mut(|fault| *fault = Some((path.clone(), kind)));
        let plan = fx.plan(Ok(Some(fx.database())), RuntimeObservation::Missing);
        assert!(
            matches!(plan, Plan::Hold(HoldReason::FileIo { .. }, ..)),
            "{kind:?}: {plan:?}"
        );
    }
}

#[test]
fn canonicalization_errors_cannot_disappear_in_a_path_fallback() {
    let fx = Fixture::new();
    fx.transcript(&fx.cwd, &fx.sid);
    let _fault = Fault;
    CANONICAL_ERROR
        .with_borrow_mut(|fault| *fault = Some((fx.cwd.clone(), io::ErrorKind::PermissionDenied)));
    let plan = fx.plan(Ok(Some(fx.database())), RuntimeObservation::Missing);
    assert!(matches!(plan, Plan::Hold(HoldReason::FileIo { .. }, ..)));
}

#[test]
fn different_exact_cwd_files_are_a_conflict_even_for_an_unselected_raw_id() {
    let fx = Fixture::new();
    let other = fx.root.path().join("other-worktree");
    fs::create_dir_all(&other).unwrap();
    fx.transcript(&fx.cwd, &fx.sid);
    let raw = uuid::Uuid::new_v4().to_string();
    fx.transcript(&fx.cwd, &raw);
    fx.transcript(&other, &raw);
    let mut database = fx.database();
    database.cwd = Some(other.display().to_string());
    database.raw_provider_session_id = Some(raw);
    let plan = fx.plan(Ok(Some(database)), RuntimeObservation::Missing);
    assert!(matches!(plan, Plan::Hold(HoldReason::Conflict(_), ..)));
}

#[test]
fn canonical_growth_selection_is_preserved_for_the_matching_raw_watermark() {
    let fx = Fixture::new();
    let cached = fx.transcript(&fx.cwd, &fx.sid);
    let old = SystemTime::now() - Duration::from_secs(3_600);
    fs::File::options()
        .write(true)
        .open(cached)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(old))
        .unwrap();
    let raw = uuid::Uuid::new_v4().to_string();
    let raw_path = fx.transcript(&fx.cwd, &raw);
    let mut database = fx.database();
    database.raw_provider_session_id = Some(raw.clone());
    database.raw_provider_transcript_len_watermark = Some(1);
    database.raw_provider_transcript_watermark_session_id = Some(raw.clone());
    let plan = fx.plan(Ok(Some(database)), RuntimeObservation::Missing);
    assert!(
        matches!(plan, Plan::Resume(source, ..) if source.session_id == raw && source.path == raw_path)
    );
}

#[test]
fn a_foreign_raw_watermark_does_not_choose_the_raw_selector() {
    let fx = Fixture::new();
    let cached = fx.transcript(&fx.cwd, &fx.sid);
    let old = SystemTime::now() - Duration::from_secs(3_600);
    fs::File::options()
        .write(true)
        .open(cached)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(old))
        .unwrap();
    let raw = uuid::Uuid::new_v4().to_string();
    fx.transcript(&fx.cwd, &raw);
    let mut database = fx.database();
    database.raw_provider_session_id = Some(raw);
    database.raw_provider_transcript_len_watermark = Some(1);
    database.raw_provider_transcript_watermark_session_id = Some("another-session".into());
    database.raw_provider_transcript_growth_proven = true;
    let plan = fx.plan(Ok(Some(database)), RuntimeObservation::Missing);
    assert!(matches!(plan, Plan::Resume(source, ..) if source.session_id == fx.sid));
}

#[test]
fn a_context_or_binding_history_read_error_is_not_a_fresh_fallback() {
    let fx = Fixture::new();
    for (context, history) in [
        (Err("unreadable".into()), Ok(Vec::new())),
        (Ok(None), Err("corrupt log".into())),
    ] {
        let plan = evaluate(
            fx.request(),
            Ok(Some(fx.database())),
            RuntimeObservation::Missing,
            context,
            history,
        );
        assert!(matches!(plan, Plan::Hold(..)));
    }
}

#[test]
fn an_expired_binding_is_not_silently_dropped_before_planning() {
    let fx = Fixture::new();
    let path = fx.transcript(&fx.cwd, &fx.sid);
    let plan = fx.plan(
        Ok(Some(fx.database())),
        RuntimeObservation::Expired(fx.binding(&path)),
    );
    assert!(matches!(plan, Plan::Hold(HoldReason::Runtime(_), ..)));
}

fn context(fx: &Fixture) -> BindingContext {
    BindingContext {
        schema: 1,
        provider: "claude".into(),
        created_at: chrono::Utc::now(),
        execution_nonce: "a".repeat(32),
        tmux_session: fx.request().logical_key.into(),
        channel_id: Some(fx.request().channel_id),
        owner_runtime_root: crate::services::tmux_common::current_tmux_owner_marker(),
        host: None,
        expected_native_session_id: Some(fx.sid.clone()),
        launch_mode: "resume".into(),
        provider_root: Some(fx.home.join("projects")),
        first_prompt_digest: None,
        source_policy: None,
    }
}

fn event(fx: &Fixture, source: SourceId, nonce: &str) -> BindingEvent {
    BindingEvent {
        seq: 1,
        channel_id: fx.request().channel_id,
        provider: "claude".into(),
        tmux_session: fx.request().logical_key.into(),
        execution_nonce: Some(nonce.into()),
        old: None,
        new: BindingTarget::Source(source),
        cause: crate::services::tui_prompt_dedupe::binding_events::BindingCause::Resume,
        parent_hint: None,
        evidence: crate::services::tui_prompt_dedupe::binding_events::BindingEvidence {
            hook_event: Some("session_start".into()),
            received_at: chrono::Utc::now(),
        },
        committed_at: chrono::Utc::now(),
    }
}

#[test]
fn context_selector_and_latest_binding_nonce_are_checked_independently() {
    let fx = Fixture::new();
    let path = fx.transcript(&fx.cwd, &fx.sid);
    let file = read_file(&path).unwrap().unwrap();
    let source = SourceId {
        session_id: fx.sid.clone(),
        path,
        dev: file.dev,
        ino: file.ino,
    };
    let own = context(&fx);
    let history = vec![event(&fx, source.clone(), &own.execution_nonce)];
    let plan = evaluate(
        fx.request(),
        Ok(Some(fx.database())),
        RuntimeObservation::Missing,
        Ok(Some(own.clone())),
        Ok(history.clone()),
    );
    assert!(matches!(plan, Plan::Resume(..)));
    let mut wrong = own.clone();
    wrong.expected_native_session_id = Some(uuid::Uuid::new_v4().to_string());
    let plan = evaluate(
        fx.request(),
        Ok(Some(fx.database())),
        RuntimeObservation::Missing,
        Ok(Some(wrong)),
        Ok(history),
    );
    assert!(matches!(plan, Plan::Hold(HoldReason::Conflict(_), ..)));
    let plan = evaluate(
        fx.request(),
        Ok(Some(fx.database())),
        RuntimeObservation::Missing,
        Ok(Some(own)),
        Ok(vec![event(&fx, source, &"b".repeat(32))]),
    );
    assert!(matches!(plan, Plan::Hold(HoldReason::Conflict(_), ..)));
}

#[test]
fn a_logged_inode_replacement_is_not_resumed_as_the_original_source() {
    let fx = Fixture::new();
    let path = fx.transcript(&fx.cwd, &fx.sid);
    let file = read_file(&path).unwrap().unwrap();
    let source = SourceId {
        session_id: fx.sid.clone(),
        path,
        dev: file.dev,
        ino: file.ino + 1,
    };
    let plan = evaluate(
        fx.request(),
        Ok(Some(fx.database())),
        RuntimeObservation::Missing,
        Ok(None),
        Ok(vec![event(&fx, source, &"a".repeat(32))]),
    );
    assert!(matches!(plan, Plan::Hold(HoldReason::Conflict(_), ..)));
}

#[test]
fn repeated_file_observations_reject_replacement_shrink_and_late_creation() {
    let fx = Fixture::new();
    let path = fx.transcript(&fx.cwd, &fx.sid);
    let file = read_file(&path).unwrap().unwrap();
    for changed in [
        FileRead {
            ino: file.ino + 1,
            ..file.clone()
        },
        FileRead {
            len: file.len - 1,
            ..file.clone()
        },
    ] {
        let mut readset = ReadSet::new(&fx.request());
        remember_file(&mut readset, &file).unwrap();
        assert!(matches!(
            remember_file(&mut readset, &changed),
            Err(HoldReason::Conflict(_))
        ));
    }
    let mut readset = ReadSet::new(&fx.request());
    readset.missing_files.push(path);
    assert!(matches!(
        remember_file(&mut readset, &file),
        Err(HoldReason::Conflict(_))
    ));
}

async fn canonical_row(pool: &PgPool, fx: &Fixture, key: &str, channel: u64) {
    sqlx::query(
        "INSERT INTO sessions (session_key, provider, status, identity_kind,
                               discord_token_hash, channel_id, cwd,
                               claude_session_id, raw_provider_session_id)
         VALUES ($1, 'claude', 'idle', 'discord_channel', $2, $3, $4, $5, $5)",
    )
    .bind(key)
    .bind(fx.request().discord_token_hash)
    .bind(channel.to_string())
    .bind(fx.cwd.display().to_string())
    .bind(&fx.sid)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn a_new_node_locator_without_an_alias_resolves_the_existing_canonical_source_pg() {
    let test_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = test_db.connect_and_migrate().await;
    let fx = Fixture::new();
    let source = fx.transcript(&fx.cwd, &fx.sid);
    let old_key = "claude/discord_0123456789abcdef/mac-mini:AgentDesk-claude-plan";
    canonical_row(&pool, &fx, old_key, fx.request().channel_id).await;
    let aliases: i64 = sqlx::query_scalar("SELECT count(*) FROM session_key_aliases")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(aliases, 0, "canonical fallback must not depend on an alias");
    let database = load_canonical_database(&pool, &fx.request())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(database.resolved_session_key, old_key);
    let plan = fx.plan(Ok(Some(database)), RuntimeObservation::Missing);
    assert!(
        matches!(&plan, Plan::Resume(selected, ..) if selected.session_id == fx.sid && selected.path == source)
    );
    let diagnostic = plan.diagnostic();
    assert_eq!(diagnostic["context"]["expected_native_session_id"], fx.sid);
    assert_eq!(
        diagnostic["readset"]["discord_token_hash"],
        fx.request().discord_token_hash
    );
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        rows, 1,
        "planning does not create a row for the new locator"
    );
    pool.close().await;
    test_db.drop().await;
}

#[tokio::test]
async fn conflicting_current_locator_and_canonical_owner_hold_the_migration_plan_pg() {
    let test_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = test_db.connect_and_migrate().await;
    let fx = Fixture::new();
    fx.transcript(&fx.cwd, &fx.sid);
    let old_key = "claude/discord_0123456789abcdef/mac-mini:AgentDesk-claude-plan";
    canonical_row(&pool, &fx, old_key, fx.request().channel_id).await;
    canonical_row(
        &pool,
        &fx,
        fx.request().session_key,
        fx.request().channel_id + 1,
    )
    .await;
    let database = load_canonical_database(&pool, &fx.request()).await;
    assert!(
        database.is_err(),
        "conflicting evidence must not use locator precedence"
    );
    let plan = fx.plan(database, RuntimeObservation::Missing);
    assert!(matches!(plan, Plan::Hold(HoldReason::Database(_), ..)));
    let owners: i64 =
        sqlx::query_scalar("SELECT count(*) FROM sessions WHERE identity_kind='discord_channel'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        owners, 2,
        "planning does not merge or delete conflicting owners"
    );
    pool.close().await;
    test_db.drop().await;
}

#[tokio::test]
async fn a_foreign_exact_locator_without_a_matching_canonical_row_is_not_adopted_pg() {
    let test_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = test_db.connect_and_migrate().await;
    let fx = Fixture::new();
    fx.transcript(&fx.cwd, &fx.sid);
    canonical_row(
        &pool,
        &fx,
        fx.request().session_key,
        fx.request().channel_id + 1,
    )
    .await;
    let database = load_canonical_database(&pool, &fx.request()).await;
    assert!(
        database.is_err(),
        "a unique exact locator still needs the requested canonical owner"
    );
    let plan = fx.plan(database, RuntimeObservation::Missing);
    assert!(matches!(plan, Plan::Hold(HoldReason::Database(_), ..)));
    let owner: String = sqlx::query_scalar("SELECT channel_id FROM sessions WHERE session_key=$1")
        .bind(fx.request().session_key)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(owner, (fx.request().channel_id + 1).to_string());
    pool.close().await;
    test_db.drop().await;
}

#[test]
fn missing_selectors_with_durable_source_pending_or_context_are_unresolved() {
    let fx = Fixture::new();
    let path = fx.transcript(&fx.cwd, &fx.sid);
    let file = read_file(&path).unwrap().unwrap();
    let source = SourceId {
        session_id: fx.sid.clone(),
        path,
        dev: file.dev,
        ino: file.ino,
    };
    let logged = event(&fx, source, &"a".repeat(32));
    let mut pending = logged.clone();
    pending.new = BindingTarget::Pending {
        payload_session_id: fx.sid.clone(),
        payload_transcript_path: None,
    };
    for (context, history) in [
        (None, vec![logged]),
        (None, vec![pending]),
        (Some(context(&fx)), Vec::new()),
    ] {
        let database = ProviderSessionIds {
            claude_session_id: None,
            raw_provider_session_id: None,
            ..fx.database()
        };
        let plan = evaluate(
            fx.request(),
            Ok(Some(database)),
            RuntimeObservation::Missing,
            Ok(context),
            Ok(history),
        );
        assert!(matches!(plan, Plan::Hold(HoldReason::Conflict(_), ..)));
    }
}

#[test]
fn planning_growth_samples_do_not_seed_or_advance_the_shared_chooser_cache() {
    let selector = uuid::Uuid::new_v4().to_string();
    let sample = |len| SelectorFileActivity {
        exists: true,
        len,
        mtime_age_secs: Some(1),
        observed_growth_since_previous_sample: false,
    };
    assert!(
        !activity_with_read_only_growth(&selector, sample(10), None, false)
            .observed_growth_since_previous_sample
    );
    assert!(
        !activity_with_read_only_growth(&selector, sample(20), None, false)
            .observed_growth_since_previous_sample
    );
    crate::services::session_selector_validity::activity_with_observed_growth(
        &selector,
        sample(10),
        None,
        false,
    );
    assert!(
        activity_with_read_only_growth(&selector, sample(20), None, false)
            .observed_growth_since_previous_sample
    );
    assert!(
        activity_with_read_only_growth(&selector, sample(15), None, false)
            .observed_growth_since_previous_sample
    );
}

#[test]
fn a_shadow_nonce_probe_neither_creates_nor_chmods_the_session_directory() {
    use std::os::unix::fs::PermissionsExt;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let sessions = crate::services::tmux_common::persistent_sessions_dir().unwrap();
    assert!(!sessions.exists());
    assert_eq!(
        observe_spawn_nonce_marker_read_only("shadow-only"),
        SpawnNonceMarker::Absent
    );
    assert!(!sessions.exists(), "a missing marker is only observed");
    fs::create_dir_all(&sessions).unwrap();
    fs::set_permissions(&sessions, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        observe_spawn_nonce_marker_read_only("shadow-only"),
        SpawnNonceMarker::Absent
    );
    assert_eq!(
        fs::metadata(sessions).unwrap().permissions().mode() & 0o777,
        0o755
    );
}
