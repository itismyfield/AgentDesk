use super::owner_input::*;
use crate::services::claude_tui::composer_lock::{self, ComposerAdmission, DraftGuard};
use crate::services::claude_tui::host_input::guarded_submission::executor_tests::{
    install_shim, renderer, screen,
};
use crate::services::tui_input::submission_tmux::guarded::{
    PaneIdentity, transport_tests::Fixture,
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

fn context(name: &str, nonce: &str) -> serde_json::Value {
    let root = crate::services::tmux_common::current_tmux_owner_marker();
    let value = serde_json::json!({"schema":1,"provider":"claude","created_at":chrono::Utc::now(),"execution_nonce":nonce,"tmux_session":name,"channel_id":727420,"owner_runtime_root":root,"host":"epoch-test","expected_native_session_id":null,"launch_mode":"fresh","provider_root":null});
    match crate::services::tui_prompt_dedupe::binding_context::PreparedIncarnation::create(
        serde_json::from_value(value).unwrap(),
    ) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => panic!("fixture context: {error}"),
    }
    let marker = crate::services::tmux_common::session_temp_path(name, "spawn_nonce");
    std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
    std::fs::write(marker, nonce).unwrap();
    serde_json::json!({"host":"tmux","owner":name,"root":root,"execution_nonce":nonce})
}
fn scoped<R>(target: serde_json::Value, run: impl FnOnce() -> R) -> R {
    with_scope(
        "claude".into(),
        target,
        727420,
        Arc::new(|_| Ok(())),
        Arc::new(|_, _| Ok(())),
        Arc::new(|_| Ok(())),
        run,
    )
}

#[test]
fn i7314_c_epoch_preserves_old_guard_and_unknown_nonce_never_writes() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::after_shared_test_env_lock();
    install_shim(&f);
    let name = "epoch-guard-owner";
    renderer(&f, name, "own", &screen(""), &screen("own"), &screen(""));
    let old = "00112233445566778899aabbccddee11";
    let new = "00112233445566778899aabbccddee12";
    let pin = PaneIdentity::observe(name).unwrap();
    scoped(context(name, old), || {
        observe_pin(name, &pin).unwrap();
        composer_lock::guard_draft(name, DraftGuard::RecoveryRequired);
        assert!(composer_lock::draft_guarded(name));
    });
    assert!(
        composer_lock::draft_guarded(name),
        "unscoped legacy writers cannot bypass a known epoch guard"
    );
    scoped(context(name, new), || {
        observe_pin(name, &pin).unwrap();
        assert_eq!(
            composer_lock::composer_admission(name, || Some(screen(""))),
            ComposerAdmission::Any
        );
    });
    scoped(context(name, old), || {
        observe_pin(name, &pin).unwrap();
        assert!(
            composer_lock::draft_guarded(name),
            "old epoch guard remains after reevaluation"
        );
    });
    let target = context(name, new);
    std::fs::remove_file(crate::services::tmux_common::session_temp_path(
        name,
        "spawn_nonce",
    ))
    .unwrap();
    let keys = AtomicUsize::new(0);
    scoped(target, || {
        assert!(observe_pin(name, &pin).is_err());
        assert!(
            payload(|| {
                keys.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
            .is_err()
        );
    });
    assert_eq!(
        keys.load(Ordering::SeqCst),
        0,
        "unknown epoch has zero content keys"
    );
    scoped(context(name, old), || {
        observe_pin(name, &pin).unwrap();
        assert!(
            composer_lock::draft_guarded(name),
            "unknown observations cannot erase old guard"
        );
    });
}

#[test]
fn owner_scope_claim_does_not_start_payload_and_is_not_inherited_by_threads() {
    let commit = Arc::new(AtomicUsize::new(0));
    let count = commit.clone();
    with_scope(
        "claude".into(),
        serde_json::json!({"host":"herdr","owner":"scope-test","execution_nonce":"known"}),
        727421,
        Arc::new(move |_| {
            count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }),
        Arc::new(|_, _| Ok(())),
        Arc::new(|_| Ok(())),
        || {
            super::submission_fence::before_first_payload().unwrap();
            assert_eq!(
                commit.load(Ordering::SeqCst),
                0,
                "custody claim cannot start payload confirmation"
            );
            assert!(
                !std::thread::spawn(active).join().unwrap(),
                "new threads need explicit owner scope installation"
            );
            assert_eq!(payload(|| Ok(17)).unwrap(), 17);
        },
    );
    assert_eq!(commit.load(Ordering::SeqCst), 1);
    assert!(!active(), "owner scope cannot leak to another input");
}
