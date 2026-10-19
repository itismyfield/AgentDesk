//! The public boot restore must really reserve and start the proven Codex watcher.

use super::*;
use crate::services::discord::router::message_handler::codex_resume_companion_queue_tests::{
    Fixture, ID,
};
use crate::services::discord::task_supervisor::watcher_spawn_observation::Scope;
use crate::services::discord::tmux::watcher_install_fixture as install;
use crate::services::discord::turn_presence::activity::Activity;
use crate::services::tui_prompt_dedupe as dedupe;
use serde_json::json;
use std::process::{Child, Command, Stdio};

/// Its own pipe keeps this fixture-owned process alive with `codex resume UUID` in argv.
/// Both Linux /proc and macOS ps therefore exercise the real process-argument reader.
struct ResumePane(Child);

impl ResumePane {
    fn start(fixture: &Fixture) -> Self {
        let mut child = Command::new("/bin/sh")
            .args(["-c", "read fixture_input\n:", "codex", "resume", ID])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("fixture-owned pane process");
        assert!(child.try_wait().unwrap().is_none());
        std::fs::write(fixture.root.join("pane.pid"), child.id().to_string()).unwrap();
        Self(child)
    }
}

impl Drop for ResumePane {
    fn drop(&mut self) {
        drop(self.0.stdin.take());
        assert!(
            self.0
                .wait()
                .expect("fixture pane must be reaped")
                .success()
        );
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

async fn assert_attached(fixture: &Fixture, submitted: &Scope, expected_offset: u64) {
    assert_eq!(
        submitted.submissions(),
        1,
        "actual boot entry must submit one supervised watcher"
    );
    assert_eq!(
        install::reader_start(fixture.channel().get()),
        Some(expected_offset)
    );
    let binding = fixture
        .shared()
        .tmux_watchers
        .channel_binding(&fixture.channel())
        .expect("live binding after boot claim");
    assert_eq!(binding.owner_channel_id, fixture.channel());
    assert_eq!(binding.tmux_session_name, fixture.install.session);
    let output = binding
        .output_path
        .as_ref()
        .expect("claimed watcher must retain its output path");
    let canonical_output = std::path::Path::new(output).canonicalize().unwrap();
    assert_eq!(canonical_output, fixture.native);
    assert_eq!(
        crate::services::tui_o::shadow::binding_reader::source_id_for(ID, &canonical_output)
            .unwrap(),
        fixture.fold().verified.unwrap().source
    );
    assert_eq!(
        fixture
            .shared()
            .tmux_watchers
            .owner_channel_for_tmux_session(&fixture.install.session),
        Some(fixture.channel())
    );
    let handle = fixture
        .shared()
        .tmux_watchers
        .get(&fixture.channel())
        .expect("actual live registry handle");
    assert_eq!(handle.tmux_session_name, fixture.install.session);
    let canonical_handle = std::path::Path::new(&handle.output_path)
        .canonicalize()
        .unwrap();
    assert_eq!(canonical_handle, fixture.native);
    assert_eq!(
        crate::services::tui_o::shadow::binding_reader::source_id_for(ID, &canonical_handle)
            .unwrap(),
        fixture.fold().verified.unwrap().source
    );
    assert!(!handle.cancel.load(std::sync::atomic::Ordering::Acquire));
    drop(handle);
    fixture.assert_queue_one().await;
    assert!(
        crate::services::discord::inflight::load_inflight_state_read_only(
            &ProviderKind::Codex,
            fixture.channel().get()
        )
        .is_none()
    );
}

async fn boot_native(channel: u64, metadata_only: bool, partial: bool, drain: bool) {
    let fixture = Fixture::new(channel, true).await;
    if metadata_only {
        let bytes = std::fs::read_to_string(&fixture.native).unwrap();
        std::fs::write(
            &fixture.native,
            format!("{}\n", bytes.lines().next().unwrap()),
        )
        .unwrap();
        crate::services::discord::health::test_support::age_file(&fixture.native, 4 * 24 * 60 * 60);
    }
    // This native-output fixture starts without an ADK wrapper file or an inflight row.
    std::fs::remove_file(&fixture.install.output).unwrap();
    let _pane = ResumePane::start(&fixture);
    fixture.cold_memory();
    fixture.recover();
    assert!(
        fixture
            .shared()
            .tmux_watchers
            .channel_binding(&fixture.channel())
            .is_none()
    );
    assert_eq!(
        dedupe::peek_tmux_runtime_binding(&fixture.install.session)
            .unwrap()
            .relay_output_path,
        None
    );
    let before = fixture.queue_identity().await;
    let complete = std::fs::metadata(&fixture.native).unwrap().len();
    if partial {
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&fixture.native)
            .unwrap()
            .write_all(b"{\"type\":\"event_msg\"")
            .unwrap();
    }
    let checkpoint = crate::services::codex_tui::session::checkpoint::at(
        &fixture.native,
        std::fs::metadata(&fixture.native).unwrap().len(),
    )
    .unwrap();
    assert_eq!(
        checkpoint, complete,
        "fixture must expose the exact complete-record boundary"
    );
    let submitted = Scope::new(fixture.shared());
    restore_tmux_watchers(&fixture.install.http, fixture.shared()).await;
    assert_attached(&fixture, &submitted, checkpoint).await;
    assert_eq!(fixture.queue_identity().await, before);
    restore_tmux_watchers(&fixture.install.http, fixture.shared()).await;
    assert_eq!(
        submitted.submissions(),
        1,
        "boot restore must reuse the live exact source"
    );
    assert_eq!(
        install::reader_start(fixture.channel().get()),
        Some(checkpoint)
    );
    if drain {
        fixture.admit_once().await;
    }
    let joined = submitted.cancel_and_join_all().await;
    assert_eq!(
        joined.len(),
        1,
        "the actual submitted watcher must complete fixture cleanup"
    );
}

#[test]
fn resume_companion_durable_proof_boot_restore_claims_once() {
    if !install::isolated(
        module_path!(),
        "resume_companion_durable_proof_boot_restore_claims_once",
    ) {
        return;
    }
    runtime().block_on(async {
        boot_native(7_061_801, true, false, false).await;
        boot_native(7_061_811, false, false, false).await;
    });
}

#[test]
fn resume_companion_attached_confirmed_turn_mode_drains_once() {
    if !install::isolated(
        module_path!(),
        "resume_companion_attached_confirmed_turn_mode_drains_once",
    ) {
        return;
    }
    runtime().block_on(boot_native(7_061_821, false, false, true));
}

#[test]
fn resume_companion_restore_checkpoint_uses_complete_boundary() {
    if !install::isolated(
        module_path!(),
        "resume_companion_restore_checkpoint_uses_complete_boundary",
    ) {
        return;
    }
    runtime().block_on(async {
        boot_native(7_061_831, false, false, false).await;
        boot_native(7_061_841, false, true, false).await;
    });
}

#[test]
fn resume_companion_cold_proof_without_candidate_does_not_claim() {
    if !install::isolated(
        module_path!(),
        "resume_companion_cold_proof_without_candidate_does_not_claim",
    ) {
        return;
    }
    runtime().block_on(async {
        let fixture = Fixture::new(7_061_851, true).await;
        fixture.recover();
        let proof = fixture.fold().verified.unwrap();
        let before = fixture.queue_identity().await;
        fixture.cold_memory();
        assert!(std::path::Path::new(&fixture.install.output).is_file());
        assert_eq!(fixture.fold().verified, Some(proof));
        let submitted = Scope::new(fixture.shared());
        restore_tmux_watchers(&fixture.install.http, fixture.shared()).await;
        assert_eq!(submitted.submissions(), 0);
        assert_eq!(install::reader_start(fixture.channel().get()), None);
        assert!(
            fixture
                .shared()
                .tmux_watchers
                .channel_binding(&fixture.channel())
                .is_none()
        );
        assert_eq!(fixture.queue_identity().await, before);
        fixture.assert_queue_one().await;
        fixture
            .assert_activity(Activity::Unknown, "watcher_unbound")
            .await;
        fixture.assert_automatic_held().await;
        assert!(submitted.cancel_and_join_all().await.is_empty());
    });
}

#[test]
fn resume_companion_restore_rejects_unlinked_wrapper_namespace() {
    if !install::isolated(
        module_path!(),
        "resume_companion_restore_rejects_unlinked_wrapper_namespace",
    ) {
        return;
    }
    runtime().block_on(async {
        let fixture = Fixture::new(7_061_861, true).await;
        fixture.recover();
        let runtime = dedupe::peek_tmux_runtime_binding(&fixture.install.session).unwrap();
        assert_eq!(runtime.relay_output_path, None);
        assert_ne!(runtime.output_path, fixture.install.output);
        std::fs::write(
            &fixture.install.output,
            format!("{}\n", json!({"type":"fixture_wrapper"})),
        )
        .unwrap();
        let native = std::fs::read(&fixture.native).unwrap();
        let before = fixture.queue_identity().await;
        let submitted = Scope::new(fixture.shared());
        restore_tmux_watchers(&fixture.install.http, fixture.shared()).await;
        assert_eq!(submitted.submissions(), 0);
        assert_eq!(install::reader_start(fixture.channel().get()), None);
        assert!(
            fixture
                .shared()
                .tmux_watchers
                .channel_binding(&fixture.channel())
                .is_none()
        );
        assert_eq!(
            dedupe::peek_tmux_runtime_binding(&fixture.install.session),
            Some(runtime)
        );
        assert_eq!(std::fs::read(&fixture.native).unwrap(), native);
        assert_eq!(fixture.queue_identity().await, before);
        fixture.assert_queue_one().await;
    });
}

#[test]
fn resume_companion_scoped_rebind_refuses_synthetic_turn_for_confirmed_cold_channel() {
    if !install::isolated(
        module_path!(),
        "resume_companion_scoped_rebind_refuses_synthetic_turn_for_confirmed_cold_channel",
    ) {
        return;
    }
    runtime().block_on(async {
        let fixture = Fixture::new(7_061_871, true).await;
        fixture.recover();
        let fold = fixture.fold();
        let marker = std::fs::read(fixture.marker()).unwrap();
        let native = std::fs::read(&fixture.native).unwrap();
        let before = fixture.queue_identity().await;
        fixture.cold_memory();
        let submitted = Scope::new(fixture.shared());
        restore_tmux_watchers(&fixture.install.http, fixture.shared()).await;
        assert_eq!(submitted.submissions(), 0, "the same cold boot fixture must miss its candidate");
        assert!(crate::services::tui_o::turn_mode::transcript_turns(fixture.channel().get()));
        let result = crate::services::discord::recovery_engine::rebind_inflight_for_channel(
            &fixture.install.http, fixture.shared(), &ProviderKind::Codex, fixture.channel().get(),
            Some(fixture.install.session.clone()),
            crate::services::discord::recovery_engine::ManualRebindOverrides::default(), None,
        ).await;
        assert!(matches!(&result, Err(crate::services::discord::recovery_engine::RebindError::Internal(reason))
            if reason == "transcript turn mode refuses synthetic rebind creation"), "{result:?}");
        assert_eq!(submitted.submissions(), 0);
        assert!(fixture.shared().tmux_watchers.channel_binding(&fixture.channel()).is_none());
        assert!(crate::services::discord::inflight::load_inflight_state_read_only(&ProviderKind::Codex, fixture.channel().get()).is_none());
        let mailbox = crate::services::discord::mailbox_snapshot(fixture.shared(), fixture.channel()).await;
        assert!(mailbox.cancel_token.is_none() && mailbox.active_user_message_id.is_none());
        assert_eq!(fixture.queue_identity().await, before);
        fixture.assert_queue_one().await;
        assert_eq!(fixture.fold(), fold);
        assert_eq!(std::fs::read(fixture.marker()).unwrap(), marker);
        assert_eq!(std::fs::read(&fixture.native).unwrap(), native);
        fixture.assert_activity(Activity::Unknown, "watcher_unbound").await;
        fixture.assert_automatic_held().await;
    });
}
