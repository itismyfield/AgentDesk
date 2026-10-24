//! A scoped writer refuses the real re-post runner before sidecar or PG admission.

use super::*;
use crate::services::codex_tui::canary::{CANARY_CHANNEL, CANARY_TMUX};
use crate::services::tui_o::cutover::test_override;
use crate::services::tui_o::repost::o_piece_delivery::admitted_keys;
use crate::services::tui_prompt_dedupe as dedupe;

struct RepostDedupeGuard {
    _guard: std::sync::MutexGuard<'static, ()>,
}

#[tokio::test]
async fn scoped_full_resume_blocks_actual_repost_before_sidecar_activation_pg() {
    if !test_override::isolated_binding_case(concat!(
        module_path!(),
        "::scoped_full_resume_blocks_actual_repost_before_sidecar_activation_pg"
    )) {
        return;
    }
    let _environment = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _dedupe = RepostDedupeGuard {
        _guard: dedupe::TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner()),
    };
    let (root, _env) = dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
    let _tmux =
        crate::services::provider_teardown::tests::test_support::FakeTmux::install(CANARY_TMUX);
    dedupe::reset_state_for_tests();
    dedupe::binding_events::set_test_root(Some(root.path()));
    let (_, old) = super::super::canary_policy::fresh_resume::proven(root.path());
    let bytes = std::fs::read(&old.path).unwrap();
    let harness = Harness::build_channel(CANARY_CHANNEL, |_| {
        vec![InitSource {
            source_id: old.clone(),
            delivery_start: bytes.len() as u64,
            prefix_hash: hex::encode(Sha256::digest(&bytes)),
        }]
    });
    harness.gate.acquired();
    let (current, fresh) = super::super::canary_policy::fresh_resume::proven(root.path());
    harness.channel().attach_source(&fresh).unwrap();
    assert!(dedupe::codex_verified_channel_delivery_allowed(
        CANARY_CHANNEL
    ));
    assert!(dedupe::codex_verified_o_source_allowed(
        CANARY_CHANNEL,
        &fresh
    ));
    assert!(!dedupe::codex_verified_o_source_allowed(
        CANARY_CHANNEL,
        &old
    ));
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    assert!(
        admitted_keys(&pool, CANARY_CHANNEL)
            .await
            .unwrap()
            .is_empty()
    );
    let io = Io::over(&harness.port);
    let runner = Runner::new(pool.clone(), BOT, run_scope());
    let sidecar = harness
        ._runtime
        .path()
        .join("repost")
        .join(format!("{CANARY_CHANNEL}.jsonl"));
    let opened = Arc::new(AtomicUsize::new(0));
    let open_count = Arc::clone(&opened);
    let transport = Arc::clone(&io);
    let inner = ChannelTurn::new(
        runner,
        Box::new(move || {
            open_count.fetch_add(1, Ordering::SeqCst);
            Some(Arc::clone(&transport))
        }),
        CANARY_CHANNEL,
        sidecar.clone(),
    );
    let turns = Arc::new(AtomicUsize::new(0));
    let mut writer = harness.writer().with_repost(Some(Box::new(Counted {
        inner,
        turns: Arc::clone(&turns),
    })));
    writer
        .suspension
        .extend(current.execution_nonce, [old.clone()]);
    assert!(!writer.suspension.is_empty());
    assert!(writer.suspension.contains(&old));
    assert!(!writer.suspension.contains(&fresh));
    for _ in 0..2 {
        tokio::time::timeout(Duration::from_secs(10), writer.repost_turn())
            .await
            .expect("the actual re-post turn must finish");
    }
    let activated = sidecar.exists();
    let sidecar_bytes = std::fs::read(&sidecar).ok();
    let admissions = admitted_keys(&pool, CANARY_CHANNEL).await.unwrap();
    let called = turns.load(Ordering::SeqCst);
    let opened = opened.load(Ordering::SeqCst);
    let reads = io.reads.load(Ordering::SeqCst);
    let wire = io.wire();
    drop(writer);
    pool.close().await;
    db.drop().await;
    dedupe::binding_events::forget_channel_for_tests(CANARY_CHANNEL);
    dedupe::binding_events::set_test_root(None);
    dedupe::reset_state_for_tests();
    assert!(
        !activated,
        "a scoped writer activated the actual sidecar: {sidecar_bytes:?}"
    );
    assert!(
        admissions.is_empty(),
        "a scoped writer admitted re-post pieces: {admissions:?}"
    );
    assert_eq!(
        called, 0,
        "a scoped writer invoked the actual ChannelTurn hook"
    );
    assert_eq!((opened, reads, wire), (0, 0, 0));
    assert!(harness.port.posts().is_empty());
}
