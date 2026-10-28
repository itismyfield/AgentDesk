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
        claude_resume: None,
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

fn source(fx: &Fixture, sid: &str) -> SourceId {
    let path = fx.transcript(&fx.cwd, sid);
    let file = read_file(&path).unwrap().unwrap();
    SourceId {
        session_id: sid.into(),
        path,
        dev: file.dev,
        ino: file.ino,
    }
}

fn transition_fixture(clear: bool) -> (Fixture, BindingContext, Vec<BindingEvent>) {
    use crate::services::tui_prompt_dedupe::binding_events::BindingCause;
    let fx = Fixture::new();
    let first = source(&fx, &uuid::Uuid::new_v4().to_string());
    let next = source(&fx, &fx.sid);
    let mut own = context(&fx);
    own.expected_native_session_id = Some(first.session_id.clone());
    let start = event(&fx, first.clone(), &own.execution_nonce);
    let mut switched = event(&fx, next.clone(), &own.execution_nonce);
    switched.seq = if clear { 3 } else { 2 };
    switched.old = Some(first.clone());
    switched.cause = if clear {
        BindingCause::Clear
    } else {
        BindingCause::Unknown
    };
    switched.evidence.hook_event = Some(
        if clear {
            "session_start"
        } else {
            "user_prompt_submit"
        }
        .into(),
    );
    let mut history = vec![start];
    if clear {
        let mut pending = switched.clone();
        pending.seq = 2;
        pending.new = BindingTarget::Pending {
            payload_session_id: next.session_id.clone(),
            payload_transcript_path: Some(next.path.display().to_string()),
        };
        history.push(pending);
        switched.new = BindingTarget::Resolved {
            pending_seq: 2,
            source: next,
        };
    }
    history.push(switched);
    (fx, own, history)
}

#[test]
fn completed_clear_and_continuation_reconcile_the_immutable_launch_selector() {
    for clear in [false, true] {
        let (fx, own, history) = transition_fixture(clear);
        let plan = evaluate_verified(
            fx.request(),
            Ok(Some(fx.database())),
            RuntimeObservation::Present(fx.binding(&fx.path(&fx.cwd, &fx.sid))),
            Ok(Some(own)),
            Ok(history
                .into_iter()
                .map(|event| StrictBindingRecord {
                    published_at: Some(event.evidence.received_at),
                    event,
                    verified: true,
                })
                .collect()),
        );
        assert!(
            matches!(&plan, Plan::Resume(source, ..) if source.session_id == fx.sid),
            "clear={clear}: {:?}",
            plan.diagnostic()
        );
    }
}

#[test]
fn a_different_old_provider_root_is_not_definite_transcript_absence() {
    let fx = Fixture::new();
    let old_home = fx.root.path().join("old-home");
    let path = old_home
        .join("projects")
        .join(encode_project_path(&fx.cwd))
        .join(format!("{}.jsonl", fx.sid));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, "{}\n").unwrap();
    let mut own = context(&fx);
    own.provider_root = Some(old_home.join("projects"));
    for new_exists in [false, true] {
        if new_exists {
            fx.transcript(&fx.cwd, &fx.sid);
        }
        let plan = evaluate(
            fx.request(),
            Ok(Some(fx.database())),
            RuntimeObservation::Missing,
            Ok(Some(own.clone())),
            Ok(Vec::new()),
        );
        assert!(
            matches!(&plan, Plan::Hold(..)),
            "new_exists={new_exists}: {:?}",
            plan.diagnostic()
        );
    }
}

#[test]
fn source_transitions_require_verified_links_pending_proof_and_one_nonce() {
    for clear in [false, true] {
        for fault in [
            "unverified",
            "missing",
            "wrong_old",
            "nonce",
            "aba",
            "pending_sid",
            "pending_path",
            "pending_seq",
        ] {
            if !clear && fault.starts_with("pending_") {
                continue;
            }
            let (fx, own, mut history) = transition_fixture(clear);
            let mut verified = true;
            match fault {
                "unverified" => verified = false,
                "missing" => {
                    history.remove(0);
                }
                "wrong_old" => history.last_mut().unwrap().old = None,
                "nonce" => history.last_mut().unwrap().execution_nonce = Some("b".repeat(32)),
                "aba" => {
                    let mut prior = history[0].clone();
                    let mut other = prior.clone();
                    other.execution_nonce = Some("b".repeat(32));
                    prior.old = None;
                    history.insert(0, other);
                    history.insert(0, prior);
                }
                "pending_sid" => {
                    if let BindingTarget::Pending {
                        payload_session_id, ..
                    } = &mut history[1].new
                    {
                        *payload_session_id = uuid::Uuid::new_v4().to_string();
                        if let BindingTarget::Pending {
                            payload_transcript_path,
                            ..
                        } = &mut history[1].new
                        {
                            *payload_transcript_path = None;
                        }
                    }
                }
                "pending_path" => {
                    if let BindingTarget::Pending {
                        payload_transcript_path,
                        ..
                    } = &mut history[1].new
                    {
                        *payload_transcript_path = Some("/wrong/source.jsonl".into());
                        if let BindingTarget::Pending {
                            payload_session_id, ..
                        } = &mut history[1].new
                        {
                            payload_session_id.clear();
                        }
                    }
                }
                "pending_seq" => {
                    if let BindingTarget::Resolved { pending_seq, .. } = &mut history[2].new {
                        *pending_seq = 99;
                    }
                }
                _ => unreachable!(),
            }
            let history = history
                .into_iter()
                .map(|event| StrictBindingRecord {
                    published_at: Some(event.evidence.received_at),
                    event,
                    verified,
                })
                .collect();
            let plan = evaluate_verified(
                fx.request(),
                Ok(Some(fx.database())),
                RuntimeObservation::Missing,
                Ok(Some(own)),
                Ok(history),
            );
            assert!(
                matches!(plan, Plan::Hold(..)),
                "clear={clear} fault={fault}: {:?}",
                plan.diagnostic()
            );
        }
    }
}

#[test]
fn an_unreadable_old_root_is_a_hold_and_same_root_absence_stays_fresh() {
    let fx = Fixture::new();
    let own = context(&fx);
    let plan = evaluate(
        fx.request(),
        Ok(Some(fx.database())),
        RuntimeObservation::Missing,
        Ok(Some(own.clone())),
        Ok(Vec::new()),
    );
    assert!(matches!(
        plan,
        Plan::Fresh(FreshReason::TranscriptAbsent, ..)
    ));
    let _fault = Fault;
    CANONICAL_ERROR.with_borrow_mut(|fault| {
        *fault = Some((
            own.provider_root.clone().unwrap(),
            io::ErrorKind::PermissionDenied,
        ))
    });
    let plan = evaluate(
        fx.request(),
        Ok(Some(fx.database())),
        RuntimeObservation::Missing,
        Ok(Some(own)),
        Ok(Vec::new()),
    );
    assert!(matches!(plan, Plan::Hold(HoldReason::FileIo { .. }, ..)));
}

#[test]
fn canonical_aliases_of_the_same_provider_root_preserve_resume() {
    let fx = Fixture::new();
    fx.transcript(&fx.cwd, &fx.sid);
    let alias = fx.root.path().join("home-alias");
    std::os::unix::fs::symlink(&fx.home, &alias).unwrap();
    let mut own = context(&fx);
    own.provider_root = Some(alias.join("projects"));
    let plan = evaluate(
        fx.request(),
        Ok(Some(fx.database())),
        RuntimeObservation::Missing,
        Ok(Some(own)),
        Ok(Vec::new()),
    );
    assert!(matches!(plan, Plan::Resume(..)));
}

#[test]
fn strict_history_keeps_verified_evidence_and_old_records_default_to_unverified() {
    use crate::services::tui_prompt_dedupe::binding_events as events;
    let fx = Fixture::new();
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            events::set_test_root(None);
        }
    }
    let _reset = Reset;
    events::set_test_root(Some(fx.root.path()));
    let dir = fx.root.path().join(events::BINDING_EVENTS_DIR);
    fs::create_dir_all(&dir).unwrap();
    let a = event(&fx, source(&fx, &fx.sid), &"a".repeat(32));
    let mut b = a.clone();
    b.seq = 2;
    let mut b = serde_json::to_value(b).unwrap();
    b["verified"] = true.into();
    let path = dir.join(format!("{}.log", fx.request().channel_id));
    fs::write(
        &path,
        format!("{}\n{}\n{{torn", serde_json::to_string(&a).unwrap(), b),
    )
    .unwrap();
    let before = fs::read(&path).unwrap();
    let records = records_strict_verified(fx.request().channel_id)
        .unwrap()
        .unwrap();
    assert_eq!(records.len(), 2);
    assert!(!records[0].verified);
    assert!(records[1].verified);
    assert_eq!(
        fs::read(&path).unwrap(),
        before,
        "strict read cannot truncate a torn tail"
    );
    b["seq"] = 4.into();
    fs::write(
        &path,
        format!("{}\n{}\n", serde_json::to_string(&a).unwrap(), b),
    )
    .unwrap();
    assert!(
        records_strict_verified(fx.request().channel_id)
            .unwrap()
            .is_err()
    );
}

#[test]
fn event_only_strict_read_keeps_legacy_metadata_compatibility() {
    use crate::services::tui_prompt_dedupe::binding_events as events;
    let fx = Fixture::new();
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            events::set_test_root(None);
        }
    }
    let _reset = Reset;
    events::set_test_root(Some(fx.root.path()));
    let dir = fx.root.path().join(events::BINDING_EVENTS_DIR);
    fs::create_dir_all(&dir).unwrap();
    let a = event(&fx, source(&fx, &fx.sid), &"a".repeat(32));
    let mut raw = serde_json::to_value(&a).unwrap();
    raw["verified"] = "old-format-extension".into();
    fs::write(
        dir.join(format!("{}.log", fx.request().channel_id)),
        format!("{raw}\n"),
    )
    .unwrap();
    assert_eq!(
        events::records_strict(fx.request().channel_id)
            .unwrap()
            .unwrap(),
        vec![a]
    );
    assert!(
        events::records_strict_verified(fx.request().channel_id)
            .unwrap()
            .is_err()
    );
}

#[test]
fn verified_native_resume_fork_and_stop_transitions_reconcile_the_launch() {
    use crate::services::tui_prompt_dedupe::binding_events::BindingCause;
    for kind in ["resume", "fork", "stop", "clear_stop", "fork_stop"] {
        let pending = kind.ends_with("_stop");
        let (fx, own, mut history) = transition_fixture(pending);
        let cause = match kind {
            "resume" => BindingCause::Resume,
            "fork" | "fork_stop" => BindingCause::Fork,
            "clear_stop" => BindingCause::Clear,
            _ => BindingCause::Unknown,
        };
        for event in history.iter_mut().skip(1) {
            event.cause = cause;
        }
        let event = history.last_mut().unwrap();
        event.evidence.hook_event = Some(
            if kind == "stop" || pending {
                "stop"
            } else {
                "session_start"
            }
            .into(),
        );
        let records = history
            .into_iter()
            .map(|event| StrictBindingRecord {
                published_at: Some(event.evidence.received_at),
                event,
                verified: true,
            })
            .collect();
        let plan = evaluate_verified(
            fx.request(),
            Ok(Some(fx.database())),
            RuntimeObservation::Missing,
            Ok(Some(own)),
            Ok(records),
        );
        assert!(
            matches!(plan, Plan::Resume(..)),
            "native {kind}: {:?}",
            plan.diagnostic()
        );
    }
}

#[test]
fn returning_to_the_initial_selector_does_not_hide_nonce_aba() {
    let (fx, own, mut history) = transition_fixture(false);
    let BindingTarget::Source(original) = history[0].new.clone() else {
        unreachable!()
    };
    let BindingTarget::Source(other) = history[1].new.clone() else {
        unreachable!()
    };
    let mut back = history[0].clone();
    back.seq = 3;
    back.old = Some(other);
    history.push(back);
    let selected = original.session_id;
    for collision in [true, false] {
        let mut db = fx.database();
        db.claude_session_id = Some(selected.clone());
        db.raw_provider_session_id = Some(selected.clone());
        history[1].execution_nonce = Some(if collision {
            "b".repeat(32)
        } else {
            own.execution_nonce.clone()
        });
        let records = history
            .clone()
            .into_iter()
            .map(|event| StrictBindingRecord {
                published_at: Some(event.evidence.received_at),
                event,
                verified: true,
            })
            .collect();
        let plan = evaluate_verified(
            fx.request(),
            Ok(Some(db)),
            RuntimeObservation::Missing,
            Ok(Some(own.clone())),
            Ok(records),
        );
        if collision {
            assert!(
                matches!(plan, Plan::Hold(..)),
                "nonce ABA: {:?}",
                plan.diagnostic()
            );
        } else {
            assert!(
                matches!(plan, Plan::Resume(..)),
                "same execution round trip: {:?}",
                plan.diagnostic()
            );
        }
    }
}

#[test]
fn an_initial_pending_resolution_can_anchor_the_launch_selector() {
    let (fx, own, history) = transition_fixture(false);
    let BindingTarget::Source(original) = history[0].new.clone() else {
        unreachable!()
    };
    let mut pending = history[0].clone();
    pending.new = BindingTarget::Pending {
        payload_session_id: original.session_id.clone(),
        payload_transcript_path: Some(original.path.display().to_string()),
    };
    let mut resolved = history[0].clone();
    resolved.seq = 2;
    resolved.new = BindingTarget::Resolved {
        pending_seq: 1,
        source: original,
    };
    resolved.evidence.hook_event = Some("stop".into());
    let mut switched = history[1].clone();
    switched.seq = 3;
    let records = vec![pending, resolved, switched]
        .into_iter()
        .map(|event| StrictBindingRecord {
            published_at: Some(event.evidence.received_at),
            event,
            verified: true,
        })
        .collect();
    let plan = evaluate_verified(
        fx.request(),
        Ok(Some(fx.database())),
        RuntimeObservation::Missing,
        Ok(Some(own)),
        Ok(records),
    );
    assert!(
        matches!(plan, Plan::Resume(..)),
        "initial pending: {:?}",
        plan.diagnostic()
    );
}

#[test]
fn pending_resolution_uses_the_existing_id_or_path_matching_policy() {
    for path_changed in [true, false] {
        let (fx, own, mut history) = transition_fixture(true);
        if let BindingTarget::Pending {
            payload_session_id,
            payload_transcript_path,
        } = &mut history[1].new
        {
            if path_changed {
                *payload_transcript_path = Some(format!("/previous-project/{}.jsonl", fx.sid));
            } else {
                *payload_session_id = String::new();
            }
        }
        let records = history
            .into_iter()
            .map(|event| StrictBindingRecord {
                published_at: Some(event.evidence.received_at),
                event,
                verified: true,
            })
            .collect();
        let plan = evaluate_verified(
            fx.request(),
            Ok(Some(fx.database())),
            RuntimeObservation::Missing,
            Ok(Some(own)),
            Ok(records),
        );
        assert!(
            matches!(plan, Plan::Resume(..)),
            "producer matching path_changed={path_changed}: {:?}",
            plan.diagnostic()
        );
    }
}

#[test]
fn rejected_records_cannot_hide_execution_nonce_reentry() {
    let (fx, own, history) = transition_fixture(false);
    let BindingTarget::Source(original) = history[0].new.clone() else {
        unreachable!()
    };
    let mut audit = history[0].clone();
    audit.seq = 2;
    audit.execution_nonce = Some("b".repeat(32));
    audit.new = BindingTarget::Rejected {
        payload_session_id: fx.sid.clone(),
        payload_transcript_path: None,
        reason: "different execution".into(),
    };
    let mut back = history[0].clone();
    back.seq = 3;
    let records = vec![history[0].clone(), audit, back]
        .into_iter()
        .map(|event| StrictBindingRecord {
            published_at: Some(event.evidence.received_at),
            event,
            verified: true,
        })
        .collect();
    let mut db = fx.database();
    db.claude_session_id = Some(original.session_id.clone());
    db.raw_provider_session_id = Some(original.session_id);
    let plan = evaluate_verified(
        fx.request(),
        Ok(Some(db)),
        RuntimeObservation::Missing,
        Ok(Some(own)),
        Ok(records),
    );
    assert!(
        matches!(plan, Plan::Hold(..)),
        "rejected nonce ABA: {:?}",
        plan.diagnostic()
    );
}

#[test]
fn an_unverified_intermediate_source_cannot_be_hidden_by_a_later_pin() {
    let (fx, own, mut history) = transition_fixture(false);
    let BindingTarget::Source(second) = history[1].new.clone() else {
        unreachable!()
    };
    let third = source(&fx, &uuid::Uuid::new_v4().to_string());
    let mut last = event(&fx, third.clone(), &own.execution_nonce);
    last.seq = 3;
    last.old = Some(second);
    last.evidence.hook_event = Some("stop".into());
    history.push(last);
    let records = history
        .into_iter()
        .enumerate()
        .map(|(i, event)| StrictBindingRecord {
            published_at: Some(event.evidence.received_at),
            event,
            verified: i != 1,
        })
        .collect();
    let mut db = fx.database();
    db.claude_session_id = Some(third.session_id.clone());
    db.raw_provider_session_id = Some(third.session_id);
    let plan = evaluate_verified(
        fx.request(),
        Ok(Some(db)),
        RuntimeObservation::Missing,
        Ok(Some(own)),
        Ok(records),
    );
    assert!(
        matches!(plan, Plan::Hold(..)),
        "unverified intermediate: {:?}",
        plan.diagnostic()
    );
}

#[test]
fn strict_snapshot_preserves_pending_and_reclaim_publication_authority() {
    for reclaims in [false, true] {
        let (fx, own, history) = transition_fixture(false);
        let BindingTarget::Source(original) = history[0].new.clone() else {
            unreachable!()
        };
        let at = chrono::Utc::now();
        let start = history[0].clone();
        let mut pending = history[1].clone();
        pending.seq = 2;
        pending.new = BindingTarget::Pending {
            payload_session_id: fx.sid.clone(),
            payload_transcript_path: Some(fx.path(&fx.cwd, &fx.sid).display().to_string()),
        };
        pending.evidence.received_at = at + chrono::Duration::seconds(2);
        let mut pin = start.clone();
        pin.seq = 3;
        pin.old = Some(original.clone());
        pin.evidence.hook_event = Some("user_prompt_submit".into());
        pin.evidence.received_at = at + chrono::Duration::seconds(4);
        let pin_at = at + chrono::Duration::seconds(if reclaims { 3 } else { 1 });
        let records = vec![
            StrictBindingRecord {
                event: start,
                verified: reclaims,
                published_at: Some(at),
            },
            StrictBindingRecord {
                event: pending,
                verified: false,
                published_at: Some(at + chrono::Duration::seconds(2)),
            },
            StrictBindingRecord {
                event: pin,
                verified: true,
                published_at: Some(pin_at),
            },
        ];
        let mut db = fx.database();
        db.claude_session_id = Some(original.session_id.clone());
        db.raw_provider_session_id = Some(original.session_id);
        let plan = evaluate_verified(
            fx.request(),
            Ok(Some(db)),
            RuntimeObservation::Missing,
            Ok(Some(own)),
            Ok(records),
        );
        if reclaims {
            assert!(
                matches!(plan, Plan::Resume(..)),
                "published current reclaim: {:?}",
                plan.diagnostic()
            );
        } else {
            assert!(
                matches!(plan, Plan::Hold(..)),
                "older first pin cannot consume Pending: {:?}",
                plan.diagnostic()
            );
        }
    }
}
