use super::*;
use crate::db::dispatched_sessions::hosted_execution::{HostedExecution, HostedState, SourceRef};
use crate::services::codex::herdr_turn::{CodexHerdrPorts, CodexHerdrTurn};
use crate::services::herdr_launch::*;
use std::sync::atomic::{AtomicUsize, Ordering};

fn stamp() -> AbsenceStamp {
    let owner = HostedOwner {
        provider: "codex".into(),
        discord_token_hash: "marker-token".into(),
        channel_id: "8057301".into(),
        logical_key: "marker-admission-executor-8057".into(),
        owner_node: "marker-node".into(),
        runtime_root: crate::config::runtime_root().unwrap().display().to_string(),
    };
    let paths = [
        tc::session_temp_path(&owner.logical_key, tc::TMUX_RUNTIME_KIND_TEMP_EXT),
        tc::legacy_tmp_session_path(&owner.logical_key, tc::TMUX_RUNTIME_KIND_TEMP_EXT),
    ];
    for path in &paths {
        std::fs::create_dir_all(std::path::Path::new(path).parent().unwrap()).unwrap();
    }
    AbsenceStamp::capture(
        &owner,
        MarkerRowExpectation::Legacy {
            owner: (&owner).into(),
            row_id: 7,
            exact_session_key: "codex/marker-token/marker-node:marker-admission-executor-8057"
                .into(),
        },
        QueueIdentity {
            primary: 8057301,
            queued_generation: 1,
            sources: vec![],
        },
        "epoch-1".into(),
    )
    .unwrap()
}

#[test]
fn cloned_marker_carrier_takes_one_context_and_refuses_a_second_executor() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let carrier = ACarrier::new(stamp(), Arc::new(|| true));
    let cloned = carrier.clone();
    assert!(carrier.take().is_ok());
    let Err(error) = cloned.take() else {
        panic!("carrier issued a second context")
    };
    assert!(error.starts_with("NotSent("));
    assert!(matches!(
        carrier.take_result(),
        Some(AStartResult::Unproved)
    ));
    assert!(carrier.take_result().is_none());
}

#[test]
fn marker_cleanup_resumes_exact_partial_unlink_and_preserves_other_execution_files() {
    let root = crate::config::TestRuntimeRootGuard::new();
    let _tmux = crate::services::tui_prompt_dedupe::binding_context::tests::fake_tmux(root.path());
    let mut expected = stamp();
    for path in &expected.paths {
        std::fs::write(&path.path, b"codex_tui").unwrap();
    }
    let nonce = tc::session_temp_path(&expected.owner.logical_key, "spawn_nonce");
    std::fs::write(&nonce, b"original-nonce").unwrap();
    expected = AbsenceStamp::capture(
        &expected.owner,
        expected.expected.clone(),
        expected.front.clone(),
        expected.epoch.clone(),
    )
    .unwrap();
    let fence = RuntimeHintFence::acquire(&expected.owner.logical_key).unwrap();
    std::fs::remove_file(&expected.paths[0].path).unwrap();
    let clean = cleanup(&expected, &fence).unwrap();
    assert!(clean.paths.iter().all(|path| !path.path.exists()));
    assert!(expected.permits_partial(&clean));
    assert_eq!(std::fs::read(nonce).unwrap(), b"original-nonce");
}

#[test]
fn marker_cleanup_keeps_a_same_bytes_replacement_instead_of_deleting_another_entry() {
    let root = crate::config::TestRuntimeRootGuard::new();
    let _tmux = crate::services::tui_prompt_dedupe::binding_context::tests::fake_tmux(root.path());
    let initial = stamp();
    let path = &initial.paths[0].path;
    std::fs::write(path, b"codex_tui").unwrap();
    let expected = AbsenceStamp::capture(
        &initial.owner,
        initial.expected,
        initial.front,
        initial.epoch,
    )
    .unwrap();
    let fence = RuntimeHintFence::acquire(&expected.owner.logical_key).unwrap();
    let retained_inode = path.with_extension("retained");
    std::fs::rename(path, &retained_inode).unwrap();
    std::fs::write(path, b"codex_tui").unwrap();
    assert!(cleanup(&expected, &fence).is_err());
    assert_eq!(std::fs::read(path).unwrap(), b"codex_tui");
    assert_eq!(std::fs::read(retained_inode).unwrap(), b"codex_tui");
}

struct NoLaunch(AtomicUsize);
impl CodexHerdrPorts for NoLaunch {
    fn launch_host(&self) -> Option<Arc<dyn HerdrLaunchHost>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        None
    }
    fn attach(
        &self,
        _: &HostedOwner,
        _: &HostedExecution,
        _: &crate::services::tui_prompt_dedupe::binding_events::SourceId,
        _: &crate::services::session_host::HerdrTarget,
    ) -> Result<bool, String> {
        panic!("refused marker input attached")
    }
    fn confirm_bound(
        &self,
        _: &HostedOwner,
        _: &HostedExecution,
        _: &crate::services::session_host::HerdrTarget,
    ) -> Result<(), String> {
        panic!("refused marker input followed up")
    }
}

#[tokio::test(flavor = "current_thread")]
async fn actual_codex_executor_refuses_marker_carriers_before_bound_or_pending_branches() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://unused/unused")
        .unwrap();
    for state in [HostedState::Bound, HostedState::Pending] {
        let stamp = stamp();
        let owner = stamp.owner.clone();
        let mut record = HostedExecution::pending(
            owner.clone(),
            "successor".into(),
            SourceRef {
                runtime_root: owner.runtime_root.clone(),
                channel: owner.channel_id.clone(),
                provider: owner.provider.clone(),
                logical_key: owner.logical_key.clone(),
                execution_nonce: "successor".into(),
                initial_source: None,
                baseline_event_seq: None,
            },
        );
        record.state = state;
        let row = HostedRecord::Known(record);
        let carrier = ACarrier::new(stamp, Arc::new(|| true));
        let ports = NoLaunch(AtomicUsize::new(0));
        let turn = CodexHerdrTurn {
            pool: &pool,
            owner,
            channel_id: 8057301,
            endpoint: endpoint(),
            row: Some(&row),
            prompt: "same kept synthetic",
            working_dir: "/tmp",
            system_prompt: None,
            allowed_tools: &[],
            model: None,
            fast_mode: None,
            goals: None,
            compact_token_limit: None,
            cancel: None,
            admission: Some(carrier.clone()),
        };
        let result =
            crate::services::codex::herdr_turn::execute(turn, &ports, std::sync::mpsc::channel().0);
        assert!(
            result.unwrap_err().starts_with("NotSent("),
            "state={state:?}"
        );
        assert!(
            matches!(
                carrier.take_result(),
                Some(AStartResult::Refused { stage: "row", .. })
            ),
            "state={state:?}"
        );
        assert_eq!(ports.0.load(Ordering::SeqCst), 0);
    }
}

fn endpoint() -> HerdrLaunchEndpoint {
    HerdrLaunchEndpoint {
        execution_node: "marker-node".into(),
        config_key: "marker-endpoint".into(),
        socket_addr: "/fixture/herdr.sock".into(),
        herdr_session: "marker-fixture".into(),
    }
}
struct Host(AtomicUsize);
impl HerdrLaunchHost for Host {
    fn mode(&self, _: &HerdrLaunchEndpoint) -> crate::config::session_hosts::EndpointMode {
        crate::config::session_hosts::EndpointMode::LegacyDedicated
    }
    fn restore_resume(
        &self,
        _: &HerdrLaunchEndpoint,
    ) -> crate::services::session_host::RestoreResume {
        crate::services::session_host::RestoreResume::Off {
            witness: crate::services::session_host::ServerWitness::for_test(1),
        }
    }
    fn create(&self, _: &HerdrCreateRequest) -> HerdrCreateOutcome {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("stale marker input created a pane")
    }
    fn launch_evidence(
        &self,
        _: &EvidenceProbe,
    ) -> Result<crate::db::dispatched_sessions::hosted_execution::ExpectedExecution, EvidenceGap>
    {
        panic!("stale marker input probed evidence")
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn actual_fenced_launch_refuses_revoked_marker_claim_before_prepare_or_pending_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let mut stamp = stamp();
    let MarkerRowExpectation::Legacy {
        row_id,
        exact_session_key,
        ..
    } = &mut stamp.expected
    else {
        unreachable!()
    };
    *row_id = sqlx::query_scalar("INSERT INTO sessions (session_key,provider,status,identity_kind,discord_token_hash,channel_id,hosted_execution) VALUES ($1,'codex','idle','discord_channel',$2,$3,NULL) RETURNING id")
        .bind(exact_session_key.as_str()).bind(&stamp.owner.discord_token_hash).bind(&stamp.owner.channel_id)
        .fetch_one(&pool).await.unwrap();
    let owner = stamp.owner.clone();
    let carrier = ACarrier::new(stamp, Arc::new(|| false));
    let host = Arc::new(Host(AtomicUsize::new(0)));
    let prepares = AtomicUsize::new(0);
    let result = launch_herdr_session_with_admission(
        &pool,
        HerdrLaunch {
            endpoint: Some(endpoint()),
            owner,
            channel_id: Some(8057301),
            expected_native_session_id: None,
            resume: false,
            provider_root: None,
        },
        |_| {
            prepares.fetch_add(1, Ordering::SeqCst);
            Err("prepare must not run".into())
        },
        host.clone(),
        carrier.take().unwrap(),
    )
    .await;
    assert!(
        matches!(result, Err(HerdrLaunchError::NotSent(_))),
        "{result:?}"
    );
    assert!(matches!(
        carrier.take_result(),
        Some(AStartResult::Refused { stage: "stale", .. })
    ));
    assert_eq!(prepares.load(Ordering::SeqCst), 0);
    assert_eq!(host.0.load(Ordering::SeqCst), 0);
    let stored: Option<serde_json::Value> = sqlx::query_scalar(
        "SELECT hosted_execution FROM sessions WHERE discord_token_hash='marker-token'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(stored, None);
    pool.close().await;
    db.drop().await;
}
