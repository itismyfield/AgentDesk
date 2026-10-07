use super::*;
use crate::services::discord::health::legacy_supervision::RetiredForTest;
use crate::services::discord::health::legacy_supervision::test_support::{
    MockDiscord, age_file, fingerprint,
};

async fn assert_retired_marker_preserved(covered: bool) {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let temp = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        temp.path(),
    );
    let shared = crate::services::discord::make_shared_data_for_tests();
    let discord = MockDiscord::start().await;
    let _http = crate::services::discord::shared_state::test_rest::install(discord.http.clone());
    let (retired, legacy) = if covered {
        (6_325_507_001, 6_325_507_002)
    } else {
        (6_325_507_003, 6_325_507_004)
    };
    let _retired = RetiredForTest::new("codex", retired);
    let aborted_at = now_ms() - ABORT_MARKER_TTL.as_millis() as u64 - 1_000;
    let commit_at = aborted_at + 1;
    let mut paths = Vec::new();
    for channel in [retired, legacy] {
        let marker = AbortedAnchorMarker {
            provider: "codex".into(),
            channel_id: channel,
            anchor_message_id: channel * 10,
            tmux_session_name: format!("test-marker-{channel}"),
            aborted_at_ms: aborted_at,
            covered_at_ms: None,
            foreign_user_msg_id: Some(channel * 10 + 1),
            foreign_started_at: Some("2026-01-01 00:00:00".into()),
            foreign_turn_start_offset: None,
            origin: MarkerOrigin::Abort,
        };
        record(&marker).unwrap();
        if covered {
            record_commit_tombstone_at(
                commit_at,
                "codex",
                &marker.tmux_session_name,
                channel,
                marker.foreign_user_msg_id.unwrap(),
                marker.foreign_started_at.as_deref().unwrap(),
            );
        }
        let path = root().unwrap().join(format!("{}.json", marker.file_stem()));
        age_file(&path, 3_600);
        paths.push(path);
    }
    let before = fingerprint(&paths[0]);
    let legacy_before = fingerprint(&paths[1]);
    let applier = shared_reaction_applier(shared);
    assert_eq!(
        sweep_expired_with_applier("codex", now_ms(), false, &|_| false, &applier).await,
        0
    );
    assert_eq!(fingerprint(&paths[0]), before);
    assert!(discord.calls_for(retired).is_empty());
    assert!(discord.calls_for(legacy).is_empty());
    assert_eq!(load_for_channel("codex", retired)[0].covered_at_ms, None);
    assert_eq!(
        load_for_channel("codex", legacy)[0].covered_at_ms,
        covered.then_some(commit_at)
    );
    if covered {
        assert_ne!(
            fingerprint(&paths[1]),
            legacy_before,
            "the cover stamp is durable"
        );
    }
    assert_eq!(
        sweep_expired_with_applier("codex", now_ms(), true, &|_| false, &applier).await,
        1
    );
    assert_eq!(fingerprint(&paths[0]), before);
    assert!(discord.calls_for(retired).is_empty());
    assert!(
        !paths[1].exists(),
        "successful reaction consumes the legacy marker"
    );
    let calls = discord.calls_for(legacy);
    let encoded = if covered { "%E2%9C%85" } else { "%E2%9A%A0" };
    assert!(
        calls
            .iter()
            .any(|call| call.starts_with("PUT ") && call.contains(encoded)),
        "expected terminal reaction: {calls:?}"
    );
}

#[tokio::test]
async fn retired_tombstone_covered_marker_skips_stamp_and_completion_reaction() {
    assert_retired_marker_preserved(true).await;
}

#[tokio::test]
async fn retired_expired_marker_skips_warning_reaction_and_consumption() {
    assert_retired_marker_preserved(false).await;
}
