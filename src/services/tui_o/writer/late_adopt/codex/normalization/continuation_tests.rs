#![cfg(unix)]
use super::*;
use crate::services::tui_o::{
    channel_policy::{Adoption, stored},
    store::{InitOrigin, InitSource, Initialized, OStore, StoreConfig},
};
use crate::services::tui_prompt_dedupe::binding_context::PreparedIncarnation;
use serde_json::json;
use sha2::Digest;
use std::io::Write;
fn append(path: &std::path::Path, value: serde_json::Value) {
    writeln!(
        std::fs::OpenOptions::new().append(true).open(path).unwrap(),
        "{value}"
    )
    .unwrap();
}
fn turn(path: &std::path::Path, id: &str, prompt: &str) {
    append(
        path,
        json!({"type":"event_msg","payload":{"type":"task_started","turn_id":id}}),
    );
    append(
        path,
        json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":prompt}]}}),
    );
}
fn end(path: &std::path::Path, id: &str) {
    append(
        path,
        json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":id}}),
    );
}
#[test]
fn resumed_turn_keeps_its_source_input_and_old_tombstone() {
    let (root, _env) = bc::tests::fixture();
    dedupe::run_serial_test(async {
        dedupe::reset_state_for_tests();
        dedupe::binding_events::set_test_root(Some(root.path()));
        let sessions = root.path().canonicalize().unwrap().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let native = "019e660d-4859-7522-9cee-8ba7c4e7c743";
        let tmux = "continuation-original";
        let path = sessions.join("resume.jsonl");
        std::fs::write(
            &path,
            format!(
                "{}\n",
                json!({"type":"session_meta","payload":{"id":native}})
            ),
        )
        .unwrap();
        turn(&path, "previous", "previous request");
        end(&path, "previous");
        let (dev, ino) = crate::services::tui_o::shadow::capture::file_identity(
            &std::fs::metadata(&path).unwrap(),
        );
        let source = SourceId {
            session_id: native.into(),
            path: path.clone(),
            dev,
            ino,
        };
        let context = BindingContext {
            schema: 1,
            provider: "codex".into(),
            created_at: chrono::Utc::now(),
            execution_nonce: uuid::Uuid::new_v4().simple().to_string(),
            tmux_session: tmux.into(),
            channel_id: Some(7651),
            owner_runtime_root: tc::current_tmux_owner_marker(),
            host: None,
            expected_native_session_id: Some(native.into()),
            launch_mode: "resume".into(),
            provider_root: Some(sessions),
            first_prompt_digest: None,
            source_policy: Some("legacy".into()),
        };
        PreparedIncarnation::create(context.clone()).unwrap();
        let marker = tc::session_temp_path(tmux, "spawn_nonce");
        std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
        std::fs::write(&marker, &context.execution_nonce).unwrap();
        dedupe::register_tmux_channel(tmux, 7651);
        dedupe::register_tmux_runtime_binding(
            tmux,
            dedupe::TuiRuntimeBinding {
                runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::CodexTui,
                output_path: path.display().to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: Some(native.into()),
                last_offset: 0,
                relay_last_offset: None,
            },
        );
        let claim = dedupe::binding_events::codex::Claim {
            session_id: native.into(),
            path: Some(path.clone()),
            evidence: dedupe::binding_events::codex::ClaimEvidence::ExplicitResume {
                executed_session_id: native.into(),
            },
        };
        dedupe::binding_events::codex::commit_claim(
            &context,
            &claim,
            dedupe::binding_events::codex::Decision::Verified(source.clone()),
            || Ok(()),
        )
        .unwrap();
        let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, root.path())
            .unwrap()
            .unwrap();
        store
            .begin_fixture_era_as(
                &Initialized {
                    channel: 7651,
                    sources: vec![InitSource {
                        source_id: source.clone(),
                        delivery_start: 0,
                        prefix_hash: hex::encode(sha2::Sha256::digest([])),
                    }],
                    initial_anchor: 0,
                    build_digest: "continuation-test".into(),
                    at: chrono::Utc::now(),
                    policy_parent: None,
                },
                InitOrigin::Fresh,
            )
            .unwrap();
        let state = stored(Some(root.path()), &std::collections::BTreeSet::from([7651]))[&7651];
        assert_eq!(state, Adoption::Committed);
        let candidate = Candidate::new(state);
        let admit = |input, prompt: &str| {
            ContinuationInput::before_submit(
                context.clone(),
                source.clone(),
                input,
                Arc::new(CancelToken::new()),
                prompt,
            )
            .unwrap()
        };
        let a = admit(1, "request a");
        turn(&path, "a", "request a");
        let a = ContinuationProof::capture(a).unwrap();
        let old = Output::new(a.identity(), tokio::time::Instant::now());
        assert!(old.reject(Reason::Invalidated));
        assert_eq!(run(&old, &a, &candidate).await, Err(Reason::Invalidated));
        end(&path, "a");
        let renamed = admit(2, "request a");
        assert!(
            ContinuationProof::capture(renamed).is_err(),
            "old bytes cannot become a new input"
        );
        let b = admit(2, "request b");
        turn(&path, "b", "request b");
        let b = ContinuationProof::capture(b).unwrap();
        let next = Output::new(b.identity(), tokio::time::Instant::now());
        assert_eq!(run(&next, &b, &candidate).await, Ok(()));
        assert_eq!(next.phase(), Phase::Adopted);
        assert_eq!(old.phase(), Phase::RejectPendingNotice(Reason::Invalidated));
        end(&path, "b");
        let closed = admit(3, "request c");
        turn(&path, "c", "request c");
        end(&path, "c");
        append(
            &path,
            json!({"type":"response_item","payload":{"type":"message","id":"late-output","role":"assistant","content":[{"type":"output_text","text":"after closed"}]}}),
        );
        assert!(
            ContinuationProof::capture(closed).is_err(),
            "untagged output after close must refuse"
        );
        let d = admit(4, "request d");
        turn(&path, "d", "request d");
        let d = ContinuationProof::capture(d).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, before.replace("request d", "wrong input")).unwrap();
        let refused = Output::new(d.identity(), tokio::time::Instant::now());
        assert_eq!(
            run(&refused, &d, &candidate).await,
            Err(Reason::SourceRejected)
        );
        std::fs::write(&path, before).unwrap();
        end(&path, "d");
        let e = admit(5, "request e");
        turn(&path, "e", "request e");
        let e = ContinuationProof::capture(e).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let backup = path.with_extension("original");
        std::fs::rename(&path, &backup).unwrap();
        std::fs::write(&path, bytes).unwrap();
        let refused = Output::new(e.identity(), tokio::time::Instant::now());
        assert_eq!(
            run(&refused, &e, &candidate).await,
            Err(Reason::SourceRejected)
        );
        std::fs::rename(&backup, &path).unwrap();
        end(&path, "e");
        let f = admit(6, "request f");
        turn(&path, "f", "request f");
        let f = ContinuationProof::capture(f).unwrap();
        let refused = Output::new(f.identity(), tokio::time::Instant::now());
        assert_eq!(
            run(&refused, &f, &Candidate::new(Adoption::Deferred)).await,
            Err(Reason::SourceRejected)
        );
        end(&path, "f");
        let f = admit(7, "request g");
        turn(&path, "g", "request g");
        let f = ContinuationProof::capture(f).unwrap();
        std::fs::write(&marker, "replacement-nonce").unwrap();
        let refused = Output::new(f.identity(), tokio::time::Instant::now());
        assert_eq!(
            run(&refused, &f, &candidate).await,
            Err(Reason::SourceRejected)
        );
        dedupe::binding_events::set_test_root(None);
        dedupe::reset_state_for_tests();
    });
}
