use super::*;

#[tokio::test]
async fn ordinary_codex_herdr_launch_stays_fresh_with_existing_native_selector() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://unused@127.0.0.1/unused")
        .unwrap();
    let owner = HostedOwner {
        provider: "codex".into(),
        discord_token_hash: "ordinary-token".into(),
        channel_id: "8128001".into(),
        logical_key: "ordinary-herdr-fresh".into(),
        owner_node: "test".into(),
        runtime_root: "/test".into(),
    };
    crate::services::tui_prompt_dedupe::register_provider_session(
        "codex",
        &owner.logical_key,
        "019e660d-4859-7522-9cee-8ba7c4e7c745",
    );
    let turn = CodexHerdrTurn {
        pool: &pool,
        owner,
        channel_id: 8128001,
        endpoint: HerdrLaunchEndpoint {
            execution_node: "test".into(),
            config_key: "test".into(),
            socket_addr: "/test/herdr.sock".into(),
            herdr_session: "test".into(),
        },
        row: None,
        prompt: "ordinary input",
        working_dir: "/tmp",
        system_prompt: None,
        allowed_tools: &[],
        model: None,
        fast_mode: None,
        goals: None,
        compact_token_limit: None,
        cancel: None,
    };
    let launch = launch_of(&turn, Some(Path::new("/test")));
    assert!(
        !launch.resume,
        "only explicit restart intent can request resume"
    );
    assert!(launch.expected_native_session_id.is_none());
}

#[cfg(unix)]
mod greeting {
    use super::super::*;
    use crate::db::dispatched_sessions::hosted_execution::{HostedExecution, SourceRef};
    use crate::services::tui_o::store::spool::Cursor;
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::MetadataExt;
    use std::path::PathBuf;

    const CHANNEL: u64 = 8_128_002;

    fn owner() -> HostedOwner {
        HostedOwner {
            provider: "codex".into(),
            discord_token_hash: "greeting-token".into(),
            channel_id: CHANNEL.to_string(),
            logical_key: "greeting-herdr-resume".into(),
            owner_node: "test".into(),
            runtime_root: "/test".into(),
        }
    }

    fn retired() -> HostedRecord {
        let owner = owner();
        let source_ref = SourceRef {
            runtime_root: owner.runtime_root.clone(),
            channel: owner.channel_id.clone(),
            provider: owner.provider.clone(),
            logical_key: owner.logical_key.clone(),
            execution_nonce: "00112233445566778899aabbccddeeff".into(),
            initial_source: None,
            baseline_event_seq: None,
        };
        let nonce = source_ref.execution_nonce.clone();
        let mut record = HostedExecution::pending(owner, nonce, source_ref);
        record.state = HostedState::Retired;
        HostedRecord::Known(record)
    }

    /// The ended rollout and the O cursor at `behind` bytes before its EOF.
    fn ended_source(behind: u64) -> (SourceId, PathBuf) {
        let runtime = crate::config::runtime_root().unwrap();
        let native = uuid::Uuid::new_v4().to_string();
        let root = runtime.join("codex/sessions");
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join(format!("rollout-{native}.jsonl"));
        let line = serde_json::json!({"type":"session_meta","payload":{
            "id":native,"source":"cli","cwd":"/tmp","originator":"codex_cli_rs"}});
        let bytes = format!("{line}\n").into_bytes();
        std::fs::write(&path, &bytes).unwrap();
        let path = path.canonicalize().unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let source = SourceId {
            session_id: native,
            path,
            dev: meta.dev(),
            ino: meta.ino(),
        };
        let dir = runtime
            .join("o_store")
            .join(CHANNEL.to_string())
            .join("cursor");
        std::fs::create_dir_all(&dir).unwrap();
        let cursor = Cursor {
            source: source.clone(),
            captured_through: meta.len() - behind,
            prefix_hash: hex::encode(Sha256::digest(&bytes)),
            retired: false,
            transition_cleanup: None,
        };
        let file = dir.join("source.json");
        std::fs::write(&file, serde_json::to_vec(&cursor).unwrap()).unwrap();
        (source, file)
    }

    fn launch(row: &HostedRecord, greeting: bool) -> HerdrLaunch {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://unused@127.0.0.1/unused")
            .unwrap();
        let turn = CodexHerdrTurn {
            pool: &pool,
            owner: owner(),
            channel_id: CHANNEL,
            endpoint: HerdrLaunchEndpoint {
                execution_node: "test".into(),
                config_key: "test".into(),
                socket_addr: "/test/herdr.sock".into(),
                herdr_session: "test".into(),
            },
            row: Some(row),
            prompt: "안녕하세요. 세션 재시작 완료.",
            working_dir: "/tmp",
            system_prompt: None,
            allowed_tools: &[],
            model: None,
            fast_mode: None,
            goals: None,
            compact_token_limit: None,
            cancel: None,
        };
        let operation = greeting.then(|| "restart-operation".to_string());
        restart_intent::scope(operation, || launch_of(&turn, Some(Path::new("/test"))))
    }

    #[tokio::test]
    async fn only_a_restart_greeting_resumes_the_ended_native_session_at_its_o_boundary() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let (source, _) = ended_source(0);
        let row = retired();
        let ordinary = launch(&row, false);
        assert!(
            !ordinary.resume && ordinary.resume_start.is_none(),
            "the same selector and predecessor never resume an ordinary input"
        );
        assert!(ordinary.expected_native_session_id.is_none());
        let greeting = launch(&row, true);
        assert!(greeting.resume, "the restart's greeting asks for a resume");
        assert_eq!(
            greeting.expected_native_session_id.as_deref(),
            Some(source.session_id.as_str())
        );
        let start = greeting.resume_start.expect("a captured boundary");
        let eof = std::fs::metadata(&source.path).unwrap().len();
        assert_eq!(
            (start.operation.as_str(), &start.path, start.offset),
            ("restart-operation", &source.path, eof)
        );
        assert_eq!((start.dev, start.ino), (source.dev, source.ino));
        let after = launch(&row, false);
        assert!(
            !after.resume,
            "the intent ends with the greeting's own turn"
        );
    }

    #[tokio::test]
    async fn a_greeting_without_a_verified_boundary_launches_fresh() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let row = retired();
        let bare = launch(&row, true);
        assert!(
            !bare.resume && bare.resume_start.is_none(),
            "no O cursor names an ended session"
        );
        assert!(bare.expected_native_session_id.is_none());
        let (_, cursor) = ended_source(1);
        let behind = launch(&row, true);
        assert!(
            !behind.resume && behind.resume_start.is_none(),
            "a cursor behind the ended EOF is never resumed from"
        );
        std::fs::remove_file(cursor).unwrap();
        ended_source(0);
        let mut live = retired();
        if let HostedRecord::Known(record) = &mut live {
            record.state = HostedState::Bound;
        }
        let unretired = launch(&live, true);
        assert!(
            !unretired.resume,
            "a predecessor that is not retired is never resumed over"
        );
    }
}
