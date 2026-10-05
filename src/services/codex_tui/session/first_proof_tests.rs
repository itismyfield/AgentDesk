#![cfg(unix)]
use super::*;
use crate::services::tui_prompt_dedupe::binding_context::{self, PreparedIncarnation};
use serde_json::json;
use sha2::{Digest, Sha256};

fn digest(prompt: &str) -> String {
    format!("sha256:{:x}", Sha256::digest(prompt.as_bytes()))
}

#[test]
fn first_prompt_digest_requires_exact_utf8_and_a_string() {
    for prompt in ["한글", "a\nb\n", &"가나다\n".repeat(700)] {
        let expected = digest(prompt);
        assert!(first_prompt_matches(Some(&expected), &json!(prompt)));
        assert!(!first_prompt_matches(
            Some(&expected),
            &json!(format!("{prompt} "))
        ));
        assert!(!first_prompt_matches(Some(&expected), &Value::Null));
        assert!(!first_prompt_matches(None, &json!(prompt)));
        for malformed in [
            expected.to_uppercase(),
            expected[7..].to_owned(),
            "sha256:0".into(),
            format!("{expected}0"),
        ] {
            assert!(!first_prompt_matches(Some(&malformed), &json!(prompt)));
        }
    }
    assert!(!first_prompt_matches(
        Some(&digest("é")),
        &json!("e\u{301}")
    ));
}

#[test]
fn first_proof_candidate_requires_canonical_current_fresh_launch_and_strict_parent() {
    let (_runtime, _guards) = binding_context::tests::fixture();
    let sessions = tempfile::tempdir().unwrap();
    let prepared = PreparedIncarnation::prepare_at(
        "codex",
        "proof-candidate-test",
        Some(42),
        None,
        false,
        Some(sessions.path().into()),
    )
    .unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let path = sessions.path().join(format!("rollout-test-{id}.jsonl"));
    let parent =
        json!({"type":"session_meta","payload":{"id":id,"cwd":"/synthetic","source":"cli"}});
    let write = |header: &Value| std::fs::write(&path, format!("{header}\n")).unwrap();
    write(&parent);
    let prompt = json!("첫 argv\n문자열");
    let expected = digest(prompt.as_str().unwrap());
    let mut launch = CodexFirstProof {
        captured: &prepared.context,
        prepared: &prepared.context,
        current_nonce: Some(&prepared.context.execution_nonce),
        verified_fresh_spawn: true,
        no_prior_claim_or_transition: true,
        event: "UserPromptSubmit",
        source: None,
        expected_digest: Some(&expected),
        prompt: &prompt,
    };
    let claim = CodexHookSourceClaim {
        session_id: &id,
        transcript_path: Some(&path),
        expected_source: CodexRolloutSource::Cli,
    };
    let accepted = codex_first_proof_candidate(&launch, &claim).unwrap();
    assert_eq!(accepted.rollout_path, path.canonicalize().unwrap());
    let canonical_bytes = std::fs::read(&prepared.path).unwrap();
    launch.expected_digest = None;
    assert!(codex_first_proof_candidate(&launch, &claim).is_err());
    launch.expected_digest = Some(&expected);
    let wrong_prompt = json!("other argv");
    launch.prompt = &wrong_prompt;
    assert!(codex_first_proof_candidate(&launch, &claim).is_err());
    launch.prompt = &prompt;
    for event in ["Stop", "PreCompact", "PostCompact"] {
        launch.event = event;
        assert!(codex_first_proof_candidate(&launch, &claim).is_err());
    }
    launch.event = "SessionStart";
    for source in [None, Some("clear"), Some("resume"), Some("fork")] {
        launch.source = source;
        assert!(codex_first_proof_candidate(&launch, &claim).is_err());
    }
    launch.source = Some("startup");
    launch.expected_digest = None;
    assert!(codex_first_proof_candidate(&launch, &claim).is_ok());
    launch.verified_fresh_spawn = false;
    assert!(codex_first_proof_candidate(&launch, &claim).is_err());
    launch.verified_fresh_spawn = true;
    launch.no_prior_claim_or_transition = false;
    assert!(codex_first_proof_candidate(&launch, &claim).is_err());
    launch.no_prior_claim_or_transition = true;
    for nonce in [None, Some("older-nonce")] {
        launch.current_nonce = nonce;
        assert!(codex_first_proof_candidate(&launch, &claim).is_err());
    }
    launch.current_nonce = Some(&prepared.context.execution_nonce);
    for field in [
        "channel", "tmux", "owner", "host", "root", "mode", "expected", "schema", "provider",
    ] {
        let mut foreign = prepared.context.clone();
        match field {
            "channel" => foreign.channel_id = Some(43),
            "tmux" => foreign.tmux_session = "foreign".into(),
            "owner" => foreign.owner_runtime_root = "foreign".into(),
            "host" => foreign.host = Some("foreign".into()),
            "root" => foreign.provider_root = Some("/foreign".into()),
            "mode" => foreign.launch_mode = "resume".into(),
            "expected" => foreign.expected_native_session_id = Some(id.clone()),
            "schema" => foreign.schema = 2,
            "provider" => foreign.provider = "claude".into(),
            _ => unreachable!(),
        }
        let foreign_launch = CodexFirstProof {
            captured: &foreign,
            ..launch
        };
        assert!(
            codex_first_proof_candidate(&foreign_launch, &claim).is_err(),
            "{field}"
        );
    }
    launch.captured = &prepared.context;
    let mut child = parent.clone();
    child["payload"]["parent_thread_id"] = json!(uuid::Uuid::new_v4().to_string());
    write(&child);
    assert!(codex_first_proof_candidate(&launch, &claim).is_err());
    child["payload"]
        .as_object_mut()
        .unwrap()
        .remove("parent_thread_id");
    child["payload"]["source"] = json!({"subagent":{"thread_spawn":{}}});
    write(&child);
    assert!(codex_first_proof_candidate(&launch, &claim).is_err());
    let mut mismatch = parent.clone();
    mismatch["payload"]["id"] = json!(uuid::Uuid::new_v4().to_string());
    write(&mismatch);
    assert!(codex_first_proof_candidate(&launch, &claim).is_err());
    write(&parent);
    let mut corrupt = prepared.context.clone();
    corrupt.channel_id = Some(43);
    std::fs::write(&prepared.path, serde_json::to_vec(&corrupt).unwrap()).unwrap();
    assert!(codex_first_proof_candidate(&launch, &claim).is_err());
    std::fs::write(&prepared.path, &canonical_bytes).unwrap();
    assert!(codex_first_proof_candidate(&launch, &claim).is_ok());
    for field in ["mode", "expected", "channel", "root"] {
        let mut invalid = prepared.context.clone();
        match field {
            "mode" => invalid.launch_mode = "resume".into(),
            "expected" => invalid.expected_native_session_id = Some(id.clone()),
            "channel" => invalid.channel_id = None,
            "root" => invalid.provider_root = None,
            _ => unreachable!(),
        }
        std::fs::write(&prepared.path, serde_json::to_vec(&invalid).unwrap()).unwrap();
        let invalid_launch = CodexFirstProof {
            captured: &invalid,
            prepared: &invalid,
            ..launch
        };
        assert!(
            codex_first_proof_candidate(&invalid_launch, &claim).is_err(),
            "{field}"
        );
    }
    std::fs::write(&prepared.path, &canonical_bytes).unwrap();
    assert_eq!(std::fs::read(&prepared.path).unwrap(), canonical_bytes);
    assert_eq!(
        std::fs::read(&path).unwrap(),
        format!("{parent}\n").as_bytes()
    );
}
