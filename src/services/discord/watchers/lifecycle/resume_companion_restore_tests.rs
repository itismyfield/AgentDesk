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

/// Restart state of an idle fresh pane: no wrapper output, no inflight row, no
/// `codex resume` argv, and only boot rehydration's binding and owner left.
async fn rehydrated_idle(channel: u64) -> Fixture {
    let fixture = Fixture::new(channel, true).await;
    std::fs::remove_file(&fixture.install.output).unwrap();
    fixture.cold_memory();
    fixture.recover();
    assert_eq!(
        crate::services::discord::tui_prompt_relay::run_codex_rehydrate_pass_for_tests(
            fixture.shared(),
            &fixture.install.session,
        ),
        Some(channel)
    );
    fixture
}

async fn assert_restore_skips(fixture: &Fixture) {
    let runtime = dedupe::peek_tmux_runtime_binding(&fixture.install.session);
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
        runtime
    );
    assert_eq!(fixture.queue_identity().await, before);
    fixture
        .assert_activity(Activity::Unknown, "watcher_unbound")
        .await;
    assert!(submitted.cancel_and_join_all().await.is_empty());
}

#[test]
fn rehydrated_idle_codex_restore_binds_watcher_and_settles_idle() {
    if !install::isolated(
        module_path!(),
        "rehydrated_idle_codex_restore_binds_watcher_and_settles_idle",
    ) {
        return;
    }
    runtime().block_on(async {
        let fixture = rehydrated_idle(7_061_881).await;
        let checkpoint = crate::services::codex_tui::session::checkpoint::at(
            &fixture.native,
            std::fs::metadata(&fixture.native).unwrap().len(),
        )
        .unwrap();
        let submitted = Scope::new(fixture.shared());
        restore_tmux_watchers(&fixture.install.http, fixture.shared()).await;
        assert_attached(&fixture, &submitted, checkpoint).await;
        fixture.assert_activity(Activity::Idle, "closed").await;
        assert_eq!(submitted.cancel_and_join_all().await.len(), 1);
    });
}

fn legacy_rollout(id: &str) -> std::path::PathBuf {
    let path = install::root()
        .join("codex/sessions")
        .join(format!("rollout-{id}.jsonl"));
    let at = chrono::Utc::now().to_rfc3339();
    let rows = [
        json!({"type":"session_meta","timestamp":at,"payload":{"id":id,"timestamp":at,
            "cwd":install::root(),"source":"cli","originator":"codex-tui"}}),
        json!({"type":"event_msg","timestamp":at,
            "payload":{"type":"task_started","turn_id":"legacy-idle"}}),
        json!({"type":"event_msg","timestamp":at,
            "payload":{"type":"task_complete","turn_id":"legacy-idle"}}),
    ];
    std::fs::write(&path, rows.map(|row| format!("{row}\n")).concat()).unwrap();
    path
}

/// A legacy idle pane after boot rehydration: its binding and owner are published, the
/// wrapper output is gone and no rollout marker is written yet.
async fn legacy_idle(
    channel: u64,
) -> (
    install::InstallFixture,
    crate::services::discord::health::test_support::MockDiscord,
    std::path::PathBuf,
) {
    let (fixture, discord) = install::fixture(ProviderKind::Codex, channel).await;
    let native = legacy_rollout(ID);
    std::fs::remove_file(&fixture.output).unwrap();
    dedupe::register_tmux_runtime_binding(
        &fixture.session,
        dedupe::TuiRuntimeBinding {
            runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::CodexTui,
            output_path: native.display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: Some(ID.into()),
            last_offset: 0,
            relay_last_offset: None,
        },
    );
    fixture
        .shared
        .tmux_watchers
        .restore_owner_channel_for_tmux_session(&fixture.session, fixture.channel);
    (fixture, discord, native)
}

fn write_marker(session: &str, rollout: &std::path::Path) {
    crate::services::codex_tui::session::write_codex_tui_rollout_marker(session, rollout, Some(ID))
        .unwrap();
}

/// A legacy pane carries no ownership proof, so only its rollout marker ties the published
/// binding to this pane.
#[test]
fn rehydrated_idle_legacy_codex_restore_requires_marker_for_bound_rollout() {
    if !install::isolated(
        module_path!(),
        "rehydrated_idle_legacy_codex_restore_requires_marker_for_bound_rollout",
    ) {
        return;
    }
    runtime().block_on(async {
        let (fixture, _discord, native) = legacy_idle(7_061_891).await;
        let other = legacy_rollout("019e660d-4859-7522-9cee-000000000001");
        let submitted = Scope::new(&fixture.shared);
        for marker in [None, Some(&other)] {
            if let Some(path) = marker {
                write_marker(&fixture.session, path);
            }
            restore_tmux_watchers(&fixture.http, &fixture.shared).await;
            assert_eq!(submitted.submissions(), 0, "marker {marker:?} is no proof");
            assert!(
                fixture
                    .shared
                    .tmux_watchers
                    .channel_binding(&fixture.channel)
                    .is_none()
            );
        }
        write_marker(&fixture.session, &native);
        restore_tmux_watchers(&fixture.http, &fixture.shared).await;
        assert_eq!(submitted.submissions(), 1);
        let bound = fixture
            .shared
            .tmux_watchers
            .channel_binding(&fixture.channel)
            .expect("the marked rollout restores the watcher");
        assert_eq!(bound.tmux_session_name, fixture.session);
        assert_eq!(
            std::path::Path::new(bound.output_path.as_ref().unwrap())
                .canonicalize()
                .unwrap(),
            native.canonicalize().unwrap()
        );
        assert_eq!(submitted.cancel_and_join_all().await.len(), 1);
    });
}

#[test]
fn rehydrated_idle_codex_restore_refuses_other_owner_channel() {
    if !install::isolated(
        module_path!(),
        "rehydrated_idle_codex_restore_refuses_other_owner_channel",
    ) {
        return;
    }
    runtime().block_on(async {
        let fixture = rehydrated_idle(7_061_901).await;
        fixture
            .shared()
            .tmux_watchers
            .restore_owner_channel_for_tmux_session(
                &fixture.install.session,
                ChannelId::new(7_061_909),
            );
        assert_restore_skips(&fixture).await;
    });
}

/// Commit re-reads the proof under source authority, so a marker lost after the scan
/// reserves no watcher and publishes nothing.
#[test]
fn rehydrated_fallback_commit_refuses_a_marker_lost_after_scan() {
    if !install::isolated(
        module_path!(),
        "rehydrated_fallback_commit_refuses_a_marker_lost_after_scan",
    ) {
        return;
    }
    runtime().block_on(async {
        let (fixture, _discord, native) = legacy_idle(7_061_911).await;
        write_marker(&fixture.session, &native);
        let fallback = codex_restore::rollout_fallback_for_rehydrated_binding(
            &ProviderKind::Codex,
            &fixture.session,
            fixture.channel,
            &fixture.shared.tmux_watchers,
        )
        .expect("the scan admits the marked binding");
        let binding = dedupe::runtime_binding_for_tmux_session(&fixture.session);
        let published = dedupe::runtime_binding_recorded_at_for_tests(&fixture.session);
        std::fs::remove_file(crate::services::tmux_common::session_temp_path(
            &fixture.session,
            crate::services::tmux_common::CODEX_TUI_ROLLOUT_MARKER_TEMP_EXT,
        ))
        .unwrap();
        let claims = std::cell::Cell::new(0);
        assert!(!codex_restore::commit_live_direct_resume_fallback(
            &fixture.session,
            fixture.channel,
            Some(fallback),
            binding,
            &native.display().to_string(),
            || {
                claims.set(claims.get() + 1);
                true
            },
        ));
        assert_eq!(claims.get(), 0);
        assert_eq!(
            dedupe::runtime_binding_recorded_at_for_tests(&fixture.session),
            published
        );
    });
}

const QUEUED_HEAD: &str = "preserve the queued instruction";
/// The input queued before boot.
const QUEUED_PROMPT: &str = "preserve the queued instruction\n한글";
const QUEUED_ANSWER: &str = "I7327 queued head answered";
const FIRST_REPLY_ID: u64 = 900_001;

fn message_reply(
    method: &axum::http::Method,
    path: &str,
    channel: u64,
    next_id: &std::sync::atomic::AtomicU64,
) -> Option<(u16, serde_json::Value)> {
    let messages = format!("/api/v10/channels/{channel}/messages");
    let id = if method == axum::http::Method::POST && path == messages {
        next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    } else if method == axum::http::Method::PATCH {
        path.strip_prefix(&format!("{messages}/"))?.parse().ok()?
    } else {
        return None;
    };
    Some((
        200,
        crate::services::discord::health::test_support::message_json(
            id,
            channel,
            1,
            "",
            "2026-10-02T00:00:00+00:00",
        ),
    ))
}

/// Replay the mock's unique POST ids and in-place edits to count visible answer messages.
fn answer_count(requests: Vec<String>) -> usize {
    let mut next_id = FIRST_REPLY_ID;
    let mut messages = std::collections::BTreeMap::new();
    for request in requests {
        let mut parts = request.splitn(3, ' ');
        let (method, path, body) = (
            parts.next().unwrap(),
            parts.next().unwrap(),
            parts.next().unwrap_or_default(),
        );
        let id = match method {
            "POST" if path.ends_with("/messages") => {
                let id = next_id;
                next_id += 1;
                id
            }
            "PATCH" | "DELETE" => match path
                .rsplit_once("/messages/")
                .and_then(|(_, id)| id.parse::<u64>().ok())
            {
                Some(id) => id,
                None => continue,
            },
            _ => continue,
        };
        if method == "DELETE" {
            messages.remove(&id);
        } else if let Some(content) =
            serde_json::from_str::<serde_json::Value>(body).unwrap()["content"].as_str()
        {
            messages.insert(id, content.to_owned());
        }
    }
    messages
        .values()
        .filter(|content| content.contains(QUEUED_ANSWER))
        .count()
}

/// The Discord stand-in a REST kickoff needs: it names the fixture channel so routing resolves.
async fn channel_discord(
    channel: u64,
) -> crate::services::discord::health::test_support::MockDiscord {
    let next_id = std::sync::atomic::AtomicU64::new(FIRST_REPLY_ID);
    crate::services::discord::health::test_support::MockDiscord::start_with(std::sync::Arc::new(
        move |method: &axum::http::Method, path: &str| {
            if let Some(reply) = message_reply(method, path, channel, &next_id) {
                return Some(reply);
            }
            (method == axum::http::Method::GET && path.ends_with(&format!("/channels/{channel}")))
                .then(|| {
                    (
                        200,
                        json!({"id": channel.to_string(), "type": 0,
                        "name": format!("wfx-{channel}-cdx"), "guild_id": "42000", "position": 0,
                        "permission_overwrites": [], "nsfw": false, "parent_id": null}),
                    )
                })
        },
    ))
    .await
}

async fn wait_for(what: &str, mut done: impl FnMut() -> bool, trace: impl Fn() -> String) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(40);
    while !done() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "{what}: {}",
            trace()
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// A legacy turn-mode Codex channel after restart: idle pane, rollout marker from its last
/// turn, no wrapper output, and one input queued before boot.
struct LegacyQueued {
    install: install::InstallFixture,
    native: std::path::PathBuf,
    _discord: crate::services::discord::health::test_support::MockDiscord,
    _confirmation: crate::services::tui_o::turn_mode::TestConfirmation,
    _ready: crate::config::TestEnvVarGuard,
    _hosts: crate::config::session_hosts::ForcedSessionHosts,
    _live: crate::config_live_reload::TestLiveGuard,
}

impl LegacyQueued {
    async fn new(channel: u64) -> Self {
        let fixture = Self::booted(channel).await;
        fixture.enqueue().await;
        fixture
    }

    /// The restarted channel before any ingress: nothing is queued yet.
    async fn booted(channel: u64) -> Self {
        let environment = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let live = crate::config_live_reload::test_live_guard(environment);
        let hosts = crate::config::session_hosts::force_for_test(None, &[]);
        let yaml = format!(
            "server: {{}}\nagents:\n  - id: legacy-fixture\n    name: legacy-fixture\n    provider: codex\n    channels:\n      codex: {{id: '{channel}', name: wfx-{channel}-cdx, runtime: tui}}\n"
        );
        let mut config: crate::config::Config = serde_yaml::from_str(&yaml).unwrap();
        config.session_hosts =
            crate::config::session_hosts::with_boot(|boot| boot.unwrap().config().clone());
        crate::services::provider_hosting::install_provider_hosting_config(&config);
        crate::config_live_reload::install(config);
        dedupe::reset_state_for_tests();
        let (install, discord) = install::fixture(ProviderKind::Codex, channel).await;
        // Boot resolves a legacy pane's channel from the runtime config file.
        let config_file = crate::runtime_layout::config_file_path(&install::root());
        std::fs::create_dir_all(config_file.parent().unwrap()).unwrap();
        std::fs::write(config_file, yaml).unwrap();
        std::fs::write(
            install::root().join("tmux-sessions"),
            format!("{}\n", install.session),
        )
        .unwrap();
        let ready = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
            "ADK_TEST_CODEX_PROMOTE_READY",
            std::ffi::OsStr::new("1"),
        );
        let native = legacy_rollout(ID);
        std::fs::remove_file(&install.output).unwrap();
        write_marker(&install.session, &native);
        // The pane's last launch left its option evidence, so a follow-up with the same options
        // reuses the pane; the compact limit is derived as intake derives it on this host.
        let codex = ProviderKind::Codex;
        let thresholds = crate::services::discord::adk_session::fetch_context_thresholds(
            install.shared.pg_pool.as_ref(),
        )
        .await;
        let compact_limit = codex
            .compact_cli_config(
                thresholds.compact_pct_for(&codex),
                codex.resolve_context_window(None),
            )
            .first()
            .map(|(_, limit)| limit.parse::<u64>().unwrap());
        crate::services::codex_tui::warm_followup::write_codex_tui_launch_options_evidence(
            &install.session,
            &crate::services::codex::CodexLaunchOptions::new("")
                .with_compact_token_limit(compact_limit)
                .with_cwd(Some(&install::root().display().to_string())),
        )
        .unwrap();
        // The binding writer's record of the last launch, which presence reads in turn mode.
        crate::services::tui_prompt_dedupe::binding_events::set_test_root(Some(&install::root()));
        crate::services::discord::turn_presence::activity::tests::log_turn_source(
            &ProviderKind::Codex,
            channel,
            &install.session,
            "legacy-launch",
            &native,
        );
        let (_, channel_name) =
            crate::services::provider::parse_provider_and_channel_from_tmux_name(&install.session)
                .unwrap();
        install.shared.core.lock().await.sessions.insert(
            install.channel,
            crate::services::discord::DiscordSession {
                session_id: Some(ID.into()),
                memento_context_loaded: false,
                memento_reflected: false,
                current_path: Some(install::root().display().to_string()),
                history: Vec::new(),
                pending_uploads: Vec::new(),
                cleared: false,
                channel_id: Some(channel),
                channel_name: Some(channel_name),
                category_name: None,
                last_active: tokio::time::Instant::now(),
                worktree: None,
                born_generation: 0,
            },
        );
        let fixture = Self {
            install,
            native,
            _discord: discord,
            _confirmation: crate::services::tui_o::turn_mode::TestConfirmation::new(channel),
            _ready: ready,
            _hosts: hosts,
            _live: live,
        };
        // Boot forgets every in-memory binding; only the pass may publish it again.
        dedupe::reset_state_for_tests();
        assert_eq!(
            crate::services::discord::tui_prompt_relay::run_codex_rehydrate_pass_for_tests(
                &fixture.install.shared,
                &fixture.install.session,
            ),
            Some(channel)
        );
        fixture
    }

    async fn enqueue(&self) {
        use crate::services::turn_orchestrator::{
            Intervention, InterventionMode, SourceMessageQueuedGeneration,
        };
        let id = poise::serenity_prelude::MessageId::new(self.install.channel.get() + 1);
        let generation = crate::services::discord::runtime_store::process_generation();
        let item = Intervention {
            author_id: poise::serenity_prelude::UserId::new(7),
            author_is_bot: false,
            message_id: id,
            queued_generation: generation,
            source_message_ids: vec![id],
            source_message_queued_generations: vec![
                SourceMessageQueuedGeneration::user_instruction(id, generation),
            ],
            source_text_segments: Vec::new(),
            text: QUEUED_PROMPT.into(),
            mode: InterventionMode::Soft,
            created_at: std::time::Instant::now(),
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
            pending_uploads: Vec::new(),
        };
        let persistence = crate::services::discord::queue_persistence_context(
            &self.install.shared,
            &ProviderKind::Codex,
            self.install.channel,
        );
        self.install
            .shared
            .mailbox(self.install.channel)
            .replace_queue(vec![item], persistence)
            .await;
        assert_eq!(self.queue_depths().await, (1, 1));
    }

    /// Memory and durable queue depth.
    async fn queue_depths(&self) -> (usize, usize) {
        let channel = self.install.channel;
        let memory =
            crate::services::discord::mailbox_snapshot(&self.install.shared, channel).await;
        let (durable, _) = crate::services::turn_orchestrator::load_channel_pending_queue_for_tests(
            &ProviderKind::Codex,
            &self.install.shared.token_hash,
            channel,
        );
        (memory.intervention_queue.len(), durable.len())
    }

    /// Observe the streaming edit before completing the turn, so both delivery phases run.
    async fn answer(
        &self,
        discord: &crate::services::discord::health::test_support::MockDiscord,
        trace: impl Fn() -> String,
    ) {
        let mut native = std::fs::OpenOptions::new()
            .append(true)
            .open(&self.native)
            .unwrap();
        let rows = queued_turn_rows();
        let (complete, streaming) = rows.split_last().unwrap();
        std::io::Write::write_all(&mut native, streaming.concat().as_bytes()).unwrap();
        wait_for(
            "the streaming answer reaches Discord",
            || answer_count(discord.requests_for(self.install.channel.get())) > 0,
            trace,
        )
        .await;
        std::io::Write::write_all(&mut native, complete.as_bytes()).unwrap();
    }

    async fn wait_for_completion(
        &self,
        discord: &crate::services::discord::health::test_support::MockDiscord,
        trace: impl Fn() -> String,
    ) {
        let post = format!("POST /api/v10/channels/{}/messages ", self.install.channel);
        wait_for(
            "the terminal completion footer reaches Discord",
            || {
                discord
                    .requests_for(self.install.channel.get())
                    .iter()
                    .any(|request| {
                        request.strip_prefix(&post).is_some_and(|body| {
                            serde_json::from_str::<serde_json::Value>(body).unwrap()["content"]
                                .as_str()
                                .is_some_and(|content| content.starts_with("-# ⏱️ "))
                        })
                    })
            },
            trace,
        )
        .await;
    }

    async fn activity(&self) -> (Activity, String) {
        let observed =
            crate::services::discord::turn_presence::activity::tests::companion_settled_activity(
                &self.install.shared,
                self.install.channel.get(),
            )
            .await;
        (observed.activity, observed.reason.to_string())
    }
}

/// The rollout rows Codex writes for the submitted queued head and its one answer.
fn queued_turn_rows() -> [String; 5] {
    let at = chrono::Utc::now().to_rfc3339();
    let message = |role: &str, kind: &str, text: &str| {
        json!({"type":"response_item","timestamp":at,"payload":{"type":"message","role":role,
            "content":[{"type":kind,"text":text}]}})
    };
    [
        json!({"type":"event_msg","timestamp":at,
            "payload":{"type":"task_started","turn_id":"i7327-boot"}}),
        message("user", "input_text", QUEUED_PROMPT),
        json!({"type":"event_msg","timestamp":at,
            "payload":{"type":"agent_message","message":QUEUED_ANSWER}}),
        message("assistant", "output_text", QUEUED_ANSWER),
        json!({"type":"event_msg","timestamp":at,"payload":{"type":"task_complete",
            "turn_id":"i7327-boot","last_agent_message":QUEUED_ANSWER}}),
    ]
    .map(|row| format!("{row}\n"))
}

/// Exactly one Enter submitted the whole queued input, Korean line included.
fn assert_submitted_once(trace: &str) {
    let submits = install::pane_submits();
    assert_eq!(submits.len(), 1, "{trace}");
    assert_eq!(submits[0].matches(QUEUED_HEAD).count(), 1, "{trace}");
    assert!(submits[0].contains(QUEUED_PROMPT), "{trace}");
}

/// Boot order of an idle legacy Codex turn-mode channel whose input was queued before boot: the
/// restored watcher lets the boot kickoff submit the queued head once, relay one answer and drain.
#[test]
fn rehydrated_idle_codex_boot_kickoff_submits_queued_head_once_and_drains() {
    if !install::isolated(
        module_path!(),
        "rehydrated_idle_codex_boot_kickoff_submits_queued_head_once_and_drains",
    ) {
        return;
    }
    runtime().block_on(async {
        let fixture = LegacyQueued::new(7_061_941).await;
        let channel = fixture.install.channel;
        let shared = fixture.install.shared.clone();
        install::ready_codex_composer(&fixture.install.session);
        let discord = channel_discord(channel.get()).await;
        let submitted = Scope::new(&shared);
        restore_tmux_watchers(&fixture.install.http, &shared).await;
        // Boot kicks after presence settled; without a watcher the presence gate holds the queue.
        let settled = fixture.activity().await;
        let http = discord.http.clone();
        let kick_shared = shared.clone();
        let kickoff = tokio::spawn(async move {
            let deps = crate::services::discord::router::IntakeDeps {
                http: &http,
                cache: None,
                ctx_for_chained_dispatch: None,
                shared: &kick_shared,
                token: "i7327-boot",
            };
            crate::services::discord::queue_dispatch::kickoff::kickoff_idle_queues_with_deps(
                &deps,
                &ProviderKind::Codex,
            )
            .await
        });
        let trace = || {
            format!(
                "presence {settled:?}; pane input {:?}; discord {:?}",
                install::pane_input(),
                discord.requests_for(channel.get()),
            )
        };
        let submitted_once = || !install::pane_submits().is_empty();
        wait_for("the queued head is submitted", submitted_once, &trace).await;
        fixture.answer(&discord, &trace).await;
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(40), kickoff)
                .await
                .unwrap()
                .unwrap(),
            1
        );
        fixture.wait_for_completion(&discord, &trace).await;
        let _ = submitted.cancel_and_join_all().await;
        assert_submitted_once(&trace());
        assert_eq!(
            answer_count(discord.requests_for(channel.get())),
            1,
            "{}",
            trace()
        );
        assert_eq!(fixture.queue_depths().await, (0, 0));
        assert_eq!(settled, (Activity::Idle, "closed".to_owned()));
    });
}

/// Which production ingress sees the restart-gap message first.
#[derive(Clone, Copy, Debug)]
enum Ingress {
    CatchUpFirst,
    GatewayFirst,
}

/// A snowflake minted 30s ago, inside the catch-up age window.
fn recent_message_id() -> u64 {
    const DISCORD_EPOCH_MS: i64 = 1_420_070_400_000;
    let at = chrono::Utc::now().timestamp_millis() - 30_000 - DISCORD_EPOCH_MS;
    (u64::try_from(at).unwrap() << 22) | 1
}

/// Discord as the restarted bot sees it: the channel, the bot user, and a history whose only
/// message is `message`, paged by its `after`/`before` cursor.
async fn ingress_discord(
    channel: u64,
    message: u64,
) -> crate::services::discord::health::test_support::MockDiscord {
    use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
    let next_id = std::sync::atomic::AtomicU64::new(FIRST_REPLY_ID);
    let mut history = discord_mock::history_message_json(message, QUEUED_PROMPT, false);
    history["channel_id"] = json!(channel.to_string());
    history["guild_id"] = json!("42000");
    let page = move |query: &str| {
        let cursor = |name: &str| {
            query
                .split('&')
                .find_map(|pair| pair.strip_prefix(name))
                .and_then(|id| id.parse::<u64>().ok())
        };
        let hidden = cursor("after=").is_some_and(|after| after >= message)
            || cursor("before=").is_some_and(|before| before <= message);
        if hidden {
            json!([])
        } else {
            json!([history.clone()])
        }
    };
    crate::services::discord::health::test_support::MockDiscord::start_with_query(
        std::sync::Arc::new(move |method: &axum::http::Method, target: &str| {
            let (path, query) = target.split_once('?').unwrap_or((target, ""));
            if let Some(reply) = message_reply(method, path, channel, &next_id) {
                return Some(reply);
            }
            if method != axum::http::Method::GET {
                return None;
            }
            if path.ends_with("/users/@me") {
                return Some((
                    200,
                    discord_mock::discord_user_json(discord_mock::BOT_ID, "queue-bot", true),
                ));
            }
            if path.ends_with(&format!("/channels/{channel}/messages")) {
                return Some((200, page(query)));
            }
            path.ends_with(&format!("/channels/{channel}")).then(|| {
                (
                    200,
                    json!({"id": channel.to_string(), "type": 0,
                    "name": format!("wfx-{channel}-cdx"), "guild_id": "42000", "position": 0,
                    "permission_overwrites": [], "nsfw": false, "parent_id": null}),
                )
            })
        }),
    )
    .await
}

/// Production gateway intake of `message` on its own task, as the shard delivers it.
async fn gateway_intake(
    ctx: &poise::serenity_prelude::Context,
    data: &crate::services::discord::Data,
    channel: u64,
    message: u64,
) {
    use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
    let mut new_message = discord_mock::user_message(message, QUEUED_PROMPT);
    new_message.channel_id = ChannelId::new(channel);
    new_message.guild_id = Some(poise::serenity_prelude::GuildId::new(42_000));
    let event = poise::serenity_prelude::FullEvent::Message { new_message };
    let ctx = ctx.clone();
    let data = crate::services::discord::Data {
        shared: data.shared.clone(),
        token: data.token.clone(),
        provider: data.provider.clone(),
    };
    tokio::spawn(async move {
        Box::pin(crate::services::discord::router::handle_event(
            &ctx, &event, &data,
        ))
        .await
    })
    .await
    .unwrap()
    .unwrap();
}

/// One restart-gap message reaches both production ingresses in `order`, then the startup
/// recovery runs in its real order; the message is submitted once, answered once and drained.
fn restart_gap_message_is_submitted_once(order: Ingress, channel: u64) {
    use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
    runtime().block_on(async {
        let fixture = LegacyQueued::booted(channel).await;
        let shared = fixture.install.shared.clone();
        let submitted = Scope::new(&shared);
        let session = fixture.install.session.clone();
        install::ready_codex_composer(&session);
        let message = recent_message_id();
        let discord = ingress_discord(channel, message).await;
        let (_, gateway_url, gateway) =
            discord_mock::start(discord_mock::DiscordMockState::new()).await;
        let ctx = discord_mock::serenity_context_over(discord.http.clone(), gateway_url).await;
        let _routing = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
            "ADK_INTAKE_ROUTING_MODE",
            std::ffi::OsStr::new("disabled"),
        );
        {
            let mut settings = shared.settings.write().await;
            settings.owner_user_id = Some(discord_mock::USER_ID);
            settings.allow_all_users = true;
        }
        shared
            .restart
            .reconcile_done
            .store(false, std::sync::atomic::Ordering::Release);
        let data = crate::services::discord::Data {
            shared: shared.clone(),
            token: "i7327-boot".to_owned(),
            provider: ProviderKind::Codex,
        };
        let http = ctx.http.clone();
        let catch_up = || {
            crate::services::discord::catch_up::catch_up_missed_messages(
                &http,
                &shared,
                &ProviderKind::Codex,
            )
        };
        match order {
            Ingress::CatchUpFirst => {
                catch_up().await;
                gateway_intake(&ctx, &data, channel, message).await;
            }
            Ingress::GatewayFirst => {
                gateway_intake(&ctx, &data, channel, message).await;
                catch_up().await;
            }
        }
        // The rest of the startup recovery in `recovery_flush.rs` order; kickoff awaits its turn.
        let boot = tokio::spawn({
            let (ctx, shared) = (ctx.clone(), shared.clone());
            async move {
                let codex = ProviderKind::Codex;
                crate::services::discord::runtime_bootstrap::restore_queued_and_inflight_for_tests(
                    &ctx.http, &shared, &codex,
                )
                .await;
                restore_tmux_watchers(&ctx.http, &shared).await;
                let kickoff =
                    crate::services::discord::queue_dispatch::kickoff::kickoff_idle_queues;
                let first = kickoff(&ctx, &shared, "i7327-boot", &codex).await;
                crate::services::discord::mark_reconcile_complete(&shared);
                (first, kickoff(&ctx, &shared, "i7327-boot", &codex).await)
            }
        });
        let trace = || {
            format!(
                "{order:?}: pane input {:?}; submits {:?}; discord {:?}",
                install::pane_input(),
                install::pane_submits(),
                discord.requests_for(channel),
            )
        };
        wait_for(
            "the restart-gap message is submitted",
            || !install::pane_submits().is_empty(),
            &trace,
        )
        .await;
        fixture.answer(&discord, &trace).await;
        let kicks = tokio::time::timeout(std::time::Duration::from_secs(40), boot)
            .await
            .unwrap()
            .unwrap();
        fixture.wait_for_completion(&discord, &trace).await;
        let _ = submitted.cancel_and_join_all().await;
        assert_submitted_once(&format!("kicks {kicks:?}; {}", trace()));
        assert_eq!(
            answer_count(discord.requests_for(channel)),
            1,
            "{}",
            trace()
        );
        assert_eq!(fixture.queue_depths().await, (0, 0), "{}", trace());
        gateway.abort();
    });
}

#[test]
fn restart_gap_message_catch_up_first_is_submitted_once_and_drains() {
    if install::isolated(
        module_path!(),
        "restart_gap_message_catch_up_first_is_submitted_once_and_drains",
    ) {
        restart_gap_message_is_submitted_once(Ingress::CatchUpFirst, 7_061_951);
    }
}

#[test]
fn restart_gap_message_gateway_first_is_submitted_once_and_drains() {
    if install::isolated(
        module_path!(),
        "restart_gap_message_gateway_first_is_submitted_once_and_drains",
    ) {
        restart_gap_message_is_submitted_once(Ingress::GatewayFirst, 7_061_961);
    }
}
