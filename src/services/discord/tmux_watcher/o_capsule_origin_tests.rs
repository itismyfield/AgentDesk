use super::o_seed_install_tests as seed_cases;
use super::*;
use crate::services::discord::health::legacy_supervision::test_support::tree_fingerprint;
use crate::services::discord::health::legacy_supervision::{RetiredForTest, is_retired};
use crate::services::discord::inflight::load_inflight_state_read_only_result;
use crate::services::discord::tmux::OOnlyInstallOutcome;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t6_d_clean_rowless_legacy_capsule_origin_prevents_o_adoption() {
    let test = "t6_d_clean_rowless_legacy_capsule_origin_prevents_o_adoption";
    if !isolated_in("o_capsule_origin_tests", test, &[]) {
        return;
    }
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (fixture, _discord) = seed_cases::fixture(632_513_001).await;
    assert!(!is_retired("claude", fixture.channel.get()));
    assert!(
        load_inflight_state_read_only_result(&CLAUDE, fixture.channel.get())
            .unwrap()
            .is_none()
    );
    let old = said("Legacy carried scalar 안 remains unfinished");
    let scalar_start = old.find('안').unwrap();
    let split = scalar_start + 1;
    seed_cases::append(&fixture, &old.as_bytes()[..split]);
    let predecessor_observed = seed_cases::observe(&fixture);
    let predecessor = seed_cases::spawn_legacy(&fixture, scalar_start as u64, None);
    seed_cases::until(
        &predecessor_observed,
        "actual split scalar decode checkpoints before text is available",
        |s| s.event_count("checkpoint") > 0 && s.event_count("outer_eof") > 0,
    )
    .await;
    seed_cases::cancel_join(&predecessor).await;
    let predecessor_seen = predecessor_observed.snapshot();
    assert!(
        predecessor_seen.decoded_chunks.is_empty(),
        "{predecessor_seen:?}"
    );
    assert!(predecessor_seen.writable_inflight_load_calls > 0);
    drop(predecessor_observed);
    assert!(
        load_inflight_state_read_only_result(&CLAUDE, fixture.channel.get())
            .unwrap()
            .is_none()
    );
    let legacy =
        cancel_handoff::capsule_observation::pending(&fixture.shared, fixture.channel).await;
    assert_eq!(legacy.len(), 1, "{legacy:?}");
    let capsule = &legacy[0];
    assert!(
        capsule.origin.is_legacy() && capsule.has_utf8_carry,
        "{legacy:?}"
    );
    assert!(
        capsule.body.is_empty() && capsule.buffer.is_empty(),
        "{legacy:?}"
    );
    assert_eq!(capsule.offset, split as u64);
    assert!(
        !capsule.has_identity && capsule.identity.is_none() && capsule.nonce.is_none(),
        "{legacy:?}"
    );
    assert!(
        !capsule.has_startup_snapshot && capsule.startup_snapshot.is_none(),
        "{legacy:?}"
    );
    assert!(
        !capsule.has_restored_seed && capsule.restored_response_seed.is_empty(),
        "{legacy:?}"
    );
    assert!(
        !capsule.has_rewind_key && capsule.rewind_attempts == 0,
        "{legacy:?}"
    );
    assert!(!capsule.has_turn_identity_for_panel, "{legacy:?}");
    assert!(capsule.utf8_state.contains("pending: [236]"), "{legacy:?}");
    assert_ne!(capsule.authority.generation_mtime_ns, 0);
    assert_eq!(capsule.opened_source, capsule.authority.source_file);
    assert_eq!(
        capsule.authority.generation_mtime_ns,
        dr::current_generation_mtime_ns(&fixture.session)
    );
    assert_eq!(
        capsule.authority.reset_incarnation,
        fixture
            .shared
            .relay_frontier_token(fixture.channel)
            .reset_incarnation
    );

    let _retired = RetiredForTest::new("claude", fixture.channel.get());
    let root = crate::services::discord::runtime_store::discord_inflight_root().unwrap();
    let before = tree_fingerprint(&root);
    let observed = seed_cases::observe(&fixture);
    assert_eq!(fixture.install().await, OOnlyInstallOutcome::Spawned);
    seed_cases::until(
        &observed,
        "O successor initial EOF or forbidden custody adoption",
        |s| s.event_count("outer_eof") > 0 || s.event_count("capsule_o_adopted") > 0,
    )
    .await;
    assert_eq!(
        observed.snapshot().event_count("capsule_o_adopted"),
        0,
        "clean Legacy payload still has Legacy provenance"
    );
    let fresh_body = "fresh O body after rowless Legacy predecessor";
    let fresh = said(fresh_body);
    seed_cases::append(&fixture, fresh.as_bytes());
    seed_cases::until(&observed, "new O source body decode", |s| {
        s.decoded_chunks.concat() == fresh
    })
    .await;
    fixture.cancel_and_join().await;
    let remaining =
        cancel_handoff::capsule_observation::pending(&fixture.shared, fixture.channel).await;
    assert_eq!(
        remaining
            .into_iter()
            .filter(|p| p.origin.is_legacy())
            .collect::<Vec<_>>(),
        legacy,
        "clean Legacy custody must be retained byte for byte"
    );
    seed_cases::assert_rowless(&observed);
    let seen = observed.snapshot();
    assert_eq!(seen.event_count("capsule_o_adopted"), 0, "{seen:?}");
    assert!(
        seen.parser_responses.iter().any(|body| body == fresh_body),
        "{seen:?}"
    );
    assert!(
        seen.parser_responses
            .iter()
            .all(|body| !body.contains("Legacy carried scalar")),
        "{seen:?}"
    );
    assert_eq!(tree_fingerprint(&root), before);
}
