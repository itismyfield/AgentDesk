use super::*;
use crate::services::tui_prompt_dedupe::binding_context::{ResumeStart, execution_context};
use crate::services::tui_prompt_dedupe::binding_events::SourceId;
use sha2::{Digest, Sha256};
use std::os::unix::fs::MetadataExt;

struct CodexHost(FakeHost);
impl HerdrLaunchHost for CodexHost {
    fn mode(&self, e: &HerdrLaunchEndpoint) -> EndpointMode {
        self.0.mode(e)
    }
    fn restore_resume(&self, e: &HerdrLaunchEndpoint) -> RestoreResume {
        self.0.restore_resume(e)
    }
    fn create(&self, r: &HerdrCreateRequest) -> HerdrCreateOutcome {
        self.0.create(r)
    }
    fn launch_evidence(&self, p: &EvidenceProbe) -> Result<ExpectedExecution, EvidenceGap> {
        self.0.launch_evidence(p).map(|mut e| {
            e.binding_provider = "codex".into();
            e
        })
    }
}

#[tokio::test]
async fn codex_restart_pending_keeps_resume_boundary_before_create_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let runtime = crate::config::runtime_root().unwrap();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let mut owner = owner(CHANNEL);
    owner.provider = "codex".into();
    let key = "codex/restart-boundary";
    let native = uuid::Uuid::new_v4().to_string();
    let root = runtime.join("codex/sessions");
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join(format!("rollout-{native}.jsonl"));
    let bytes = format!(
        "{}\n",
        serde_json::json!({"type":"session_meta","payload":{
        "id":native,"source":"cli","cwd":"/tmp","originator":"codex_cli_rs"}})
    )
    .into_bytes();
    std::fs::write(&path, &bytes).unwrap();
    let path = path.canonicalize().unwrap();
    let meta = std::fs::metadata(&path).unwrap();
    let source = SourceId {
        session_id: native.clone(),
        path: path.clone(),
        dev: meta.dev(),
        ino: meta.ino(),
    };
    let cursor_dir = runtime.join("o_store").join(CHANNEL).join("cursor");
    std::fs::create_dir_all(&cursor_dir).unwrap();
    let cursor_path = cursor_dir.join("source.json");
    let cursor = crate::services::tui_o::store::spool::Cursor {
        source: source.clone(),
        captured_through: meta.len(),
        prefix_hash: hex::encode(Sha256::digest(&bytes)),
        retired: false,
        transition_cleanup: None,
    };
    std::fs::write(&cursor_path, serde_json::to_vec(&cursor).unwrap()).unwrap();
    let store = crate::services::tui_o::store::OStore::existing(&runtime).unwrap();
    let start = ResumeStart::capture(
        "explicit-restart".into(),
        CHANNEL.parse().unwrap(),
        &source,
        &store,
    )
    .unwrap();
    let mut old = record(
        &owner,
        "ffeeddccbbaa00998877665544332211",
        HostedState::Retired,
    );
    old.expected.as_mut().unwrap().binding_provider = "codex".into();
    sqlx::query("INSERT INTO sessions (session_key,provider,status,identity_kind,discord_token_hash,channel_id,hosted_execution) VALUES ($1,'codex','idle','discord_channel',$2,$3,$4)")
        .bind(key).bind(&owner.discord_token_hash).bind(CHANNEL).bind(serde_json::to_value(&old).unwrap()).execute(&pool).await.unwrap();
    let host = Arc::new(CodexHost(FakeHost::created("resume-pane")));
    let request = |start| HerdrLaunch {
        endpoint: Some(endpoint()),
        owner: owner.clone(),
        channel_id: Some(CHANNEL.parse().unwrap()),
        expected_native_session_id: Some(native.clone()),
        resume: true,
        resume_start: start,
        provider_root: Some(root.clone()),
    };
    let missing = launch_herdr_session(&pool, request(None), command, host.clone()).await;
    assert!(matches!(missing, Err(HerdrLaunchError::Prepare(_))));
    assert_eq!(host.0.creates.load(Ordering::SeqCst), 0);
    let mut behind = cursor.clone();
    behind.captured_through -= 1;
    std::fs::write(&cursor_path, serde_json::to_vec(&behind).unwrap()).unwrap();
    let drifted =
        launch_herdr_session(&pool, request(Some(start.clone())), command, host.clone()).await;
    assert!(matches!(drifted, Err(HerdrLaunchError::Prepare(_))));
    assert_eq!(host.0.creates.load(Ordering::SeqCst), 0);
    std::fs::write(&cursor_path, serde_json::to_vec(&cursor).unwrap()).unwrap();
    let prepared_boundary = start.clone();
    let outcome = launch_herdr_session(
        &pool,
        request(Some(start.clone())),
        move |prepared| {
            assert_eq!(
                prepared.context.resume_start,
                Some(prepared_boundary.clone())
            );
            assert_eq!(
                execution_context("codex", &prepared.context.execution_nonce).unwrap(),
                prepared.context
            );
            command(prepared)
        },
        host.clone(),
    )
    .await
    .unwrap();
    let HerdrLaunchOutcome::Launched {
        execution_nonce,
        evidence: Ok(()),
        ..
    } = outcome
    else {
        panic!("{outcome:?}")
    };
    let raw: Value =
        sqlx::query_scalar("SELECT hosted_execution FROM sessions WHERE session_key=$1")
            .bind(key)
            .fetch_one(&pool)
            .await
            .unwrap();
    let HostedRecord::Known(pending) = HostedRecord::decode(Some(&raw)) else {
        panic!("known successor")
    };
    assert_eq!(pending.state, HostedState::Pending);
    assert_eq!(pending.execution_nonce, execution_nonce);
    assert_eq!(
        execution_context("codex", &execution_nonce)
            .unwrap()
            .resume_start,
        Some(start)
    );
    assert_eq!(host.0.creates.load(Ordering::SeqCst), 1);
    pool.close().await;
    db.drop().await;
}
