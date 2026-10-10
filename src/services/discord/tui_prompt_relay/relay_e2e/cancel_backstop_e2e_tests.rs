//! Cancelled foreground ownership through real intake, dequeue, provider and relay.

#[path = "cancel_backstop_e2e_tests/autoheal.rs"]
mod autoheal;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::discord_mock::USER_ID;
use super::stop_command_catch_up_e2e::{hold_a_turn, queued_ids, recent_snowflake_base};
use super::{AbortOnDrop, RelayE2eHarness, wait_until};
use crate::services::discord::{self as discord, inflight, mailbox_snapshot, queue_io};
use crate::services::provider::{CancelToken, ProviderKind};

const WAIT: Duration = Duration::from_secs(15);
const Q: &str = "i6016 preserved queued question [cancel-reclaim-q]";
const HEAD: &str = "i6016 previous turn head already visible";
const TAIL: &str = "i6016 previous turn undelivered tail";
const CLAUDE: ProviderKind = ProviderKind::Claude;

async fn harness() -> RelayE2eHarness {
    let h = RelayE2eHarness::start_with_health_registry().await;
    h.register_channel_in_role_map();
    h.cache_relay_transport();
    h.shared
        .restart
        .reconcile_done
        .store(true, Ordering::SeqCst);
    h.health_registry
        .as_ref()
        .unwrap()
        .register("claude".to_string(), h.shared.clone())
        .await;
    h
}

fn isolated(test: &str) -> bool {
    crate::services::tui_o::cutover::test_override::in_empty_list_process(test)
}

// Debug intake and restore futures need room on both their polling thread and runtime workers.
fn run_with_large_stack<F: std::future::Future<Output = ()> + 'static>(
    scenario: impl FnOnce() -> F + Send + 'static,
) {
    std::thread::Builder::new()
        .name("cancel-backstop-e2e".to_string())
        .stack_size(16 << 20)
        .spawn(move || {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(4)
                .thread_stack_size(16 << 20)
                .enable_all()
                .build()
                .unwrap()
                .block_on(scenario());
        })
        .unwrap()
        .join()
        .unwrap();
}

fn fixture_readiness(
    h: &RelayE2eHarness,
    tmux: &str,
    kind: crate::services::agent_protocol::RuntimeHandoffKind,
    transcript: Option<&Path>,
) {
    let canonical = PathBuf::from(crate::services::tmux_common::session_temp_path(
        tmux, "jsonl",
    ));
    assert!(
        canonical.starts_with(h.root.path()),
        "readiness must stay in the isolated runtime root"
    );
    std::fs::create_dir_all(canonical.parent().unwrap()).unwrap();
    if let Some(transcript) = transcript {
        assert!(
            transcript.starts_with(h.root.path()),
            "native transcript must stay in the fixture root"
        );
        std::os::unix::fs::symlink(transcript, &canonical).unwrap();
    }
    crate::services::tmux_common::write_tmux_runtime_kind_marker(tmux, kind).unwrap();
    eprintln!(
        "I6016_FIXTURE_READINESS root={} canonical={} kind={}",
        h.root.path().display(),
        canonical.display(),
        kind.as_str()
    );
}

fn seed_row(
    h: &RelayE2eHarness,
    active: u64,
    token: &CancelToken,
    tmux: Option<&str>,
    output: Option<&Path>,
) -> inflight::InflightTurnState {
    let mut row = inflight::InflightTurnState::new(
        CLAUDE,
        h.channel_id.get(),
        None,
        USER_ID,
        active,
        0,
        "previous turn before its bridge disappeared".to_string(),
        None,
        tmux.map(str::to_string),
        output.map(|path| path.display().to_string()),
        None,
        0,
    );
    row.turn_nonce = token.turn_nonce().map(str::to_string);
    row.turn_start_offset = Some(0);
    inflight::save_inflight_state(&row).expect("seed the isolated inflight row");
    inflight::load_inflight_state_read_only(&CLAUDE, h.channel_id.get())
        .expect("the fixture row was persisted")
}

fn write_row(row: &inflight::InflightTurnState) {
    let root = discord::runtime_store::discord_inflight_root().expect("isolated inflight root");
    let path = inflight::inflight_state_path(&root, &CLAUDE, row.channel_id);
    std::fs::write(path, serde_json::to_vec_pretty(row).unwrap()).expect("write fixture row");
}

fn clear_row(h: &RelayE2eHarness, active: u64) {
    assert_eq!(
        inflight::clear_inflight_state_if_matches(&CLAUDE, h.channel_id.get(), active),
        inflight::GuardedClearOutcome::Cleared,
        "only the previous episode's fixture may be removed"
    );
}

// The placeholder rendezvous precedes spawn_turn_bridge; the persisted row is a separate fixture.
async fn abort_held_intake<T>(mut held: AbortOnDrop<T>) {
    let task = held.0.take().expect("placeholder-held intake task");
    task.abort();
    match task.await {
        Err(error) => assert!(error.is_cancelled(), "held intake actually ended"),
        Ok(_) => panic!("the fixture intake exited before the abort"),
    }
}

async fn stop_and_abort(
    h: &RelayE2eHarness,
    token: &Arc<CancelToken>,
    held: AbortOnDrop<Result<(), discord::Error>>,
    stop: u64,
) {
    h.deliver_user_message(stop, "!stop")
        .await
        .expect("production intake consumes !stop");
    assert!(token.cancelled.load(Ordering::SeqCst));
    abort_held_intake(held).await;
    let snapshot = h.mailbox().await;
    assert!(
        snapshot
            .cancel_token
            .as_ref()
            .is_some_and(|now| Arc::ptr_eq(now, token)),
        "an aborted placeholder-held intake leaves its cancelled foreground anchor"
    );
    assert!(
        h.provider_inputs().is_empty(),
        "the placeholder-held intake never reached the provider"
    );
    h.answer_placeholders_immediately();
    assert!(
        wait_until(WAIT, {
            let shared = h.shared.clone();
            let channel = h.channel_id;
            move || {
                let shared = shared.clone();
                Box::pin(
                    async move { shared.restart.deferred_hook_channels.contains_key(&channel) },
                )
            }
        })
        .await,
        "production !stop drain armed the backstop before any test wake"
    );
}

fn wake_existing(h: &RelayE2eHarness) {
    let before = h
        .shared
        .restart
        .deferred_hook_channels
        .get(&h.channel_id)
        .expect("the production cancel path already armed its backstop")
        .value()
        .clone();
    discord::schedule_deferred_idle_queue_kickoff_immediate(
        h.shared.clone(),
        CLAUDE,
        h.channel_id,
        "cancel_reclaim_e2e_wake",
    );
    let after = h
        .shared
        .restart
        .deferred_hook_channels
        .get(&h.channel_id)
        .expect("the wake reuses the existing task")
        .value()
        .clone();
    assert!(
        Arc::ptr_eq(&before, &after),
        "immediate only wakes the existing owner"
    );
}

async fn held_fire(h: &RelayE2eHarness) {
    assert!(
        wait_until(WAIT, {
            let channel = h.channel_id;
            move || {
                Box::pin(async move {
                    queue_io::cancel_backstop_test_support::backstop_waiting(channel)
                })
            }
        })
        .await,
        "the production backstop reached its slow wait"
    );
    let owner = h
        .shared
        .restart
        .deferred_hook_channels
        .get(&h.channel_id)
        .unwrap()
        .value()
        .clone();
    let before = queue_io::cancel_backstop_test_support::completed_fires(h.channel_id);
    wake_existing(h);
    let completed = wait_until(WAIT, {
        let channel = h.channel_id;
        move || {
            Box::pin(async move {
                queue_io::cancel_backstop_test_support::completed_fires(channel) > before
            })
        }
    })
    .await;
    assert!(
        completed,
        "the awakened backstop completed a real evaluation"
    );
    let after = h
        .shared
        .restart
        .deferred_hook_channels
        .get(&h.channel_id)
        .expect("a held cancelled anchor keeps a recovery owner after its fire")
        .value()
        .clone();
    assert!(
        Arc::ptr_eq(&owner, &after),
        "the same owner survives a held fire"
    );
}

async fn answered_once(h: &RelayE2eHarness) {
    let completed = wait_until(WAIT, {
        let shared = h.shared.clone();
        let channel = h.channel_id;
        let messages = h.mock.messages.clone();
        move || {
            let shared = shared.clone();
            let messages = messages.clone();
            Box::pin(async move {
                let snapshot = mailbox_snapshot(&shared, channel).await;
                snapshot.cancel_token.is_none()
                    && snapshot.intervention_queue.is_empty()
                    && snapshot.pending_user_dispatch.is_none()
                    && messages
                        .lock()
                        .unwrap()
                        .values()
                        .any(|(_, text)| text == "ok")
            })
        }
    })
    .await;
    assert!(
        completed,
        "Q must execute and answer: inputs={:?}; messages={:?}; queue={:?}",
        h.provider_inputs(),
        h.messages(),
        queued_ids(h).await
    );
    assert!(
        h.root.path().join(super::PROVIDER_INPUTS_FILE).is_file(),
        "actual provider delivery must create its recording file"
    );
    let inputs = h.provider_inputs();
    assert!(
        inputs.concat().matches(Q).count() >= 1,
        "the provider must have received Q: {inputs:?}"
    );
    assert_eq!(
        inputs.concat().matches(Q).count(),
        1,
        "Q input must occur exactly once: {inputs:?}"
    );
    assert_eq!(
        h.messages().iter().filter(|(_, text)| text == "ok").count(),
        1,
        "Q answer must remain visible exactly once"
    );
    assert!(h.durable_queue().is_empty());
    assert!(
        h.unhandled_requests().is_empty(),
        "mock must answer every production request: {:?}",
        h.unhandled_requests()
    );
}

#[test]
fn bang_stop_with_dead_bridge_parks_then_owner_runs_q_once() {
    if !isolated(concat!(
        module_path!(),
        "::bang_stop_with_dead_bridge_parks_then_owner_runs_q_once"
    )) {
        return;
    }
    run_with_large_stack(|| async {
        let h = harness().await;
        let base = recent_snowflake_base();
        let (active, queued, stop) = (base | 1, base | 2, base | 3);
        let bridge =
            discord::turn_bridge::cancel_backstop_test_support::BridgePause::install(h.channel_id);
        h.answer_placeholders_immediately();
        h.deliver_user_message(active, "long running work")
            .await
            .unwrap();
        assert!(
            bridge.wait_for_entry(WAIT).await,
            "production intake spawned a real bridge and it entered its task"
        );
        let token = h
            .mailbox()
            .await
            .cancel_token
            .expect("real foreground owner");
        let row = inflight::load_inflight_state_read_only(&CLAUDE, h.channel_id.get())
            .expect("production intake persisted inflight before handing off to the real bridge");
        assert_eq!(row.user_msg_id, active);
        assert!(row.tmux_session_name.is_none());
        let tmux = CLAUDE.build_tmux_session_name("i6016-cancelled-anchor");
        let pane = ProbePane::new(&h, &tmux);
        pane.pane("dead");
        h.deliver_user_message(queued, Q).await.unwrap();
        h.deliver_user_message(stop, "!stop").await.unwrap();
        assert!(token.cancelled.load(Ordering::SeqCst));
        assert!(
            bridge.abort_and_wait(WAIT).await,
            "the fixture-owned actual bridge ended after !stop"
        );
        drop(bridge);
        assert!(
            h.mailbox()
                .await
                .cancel_token
                .as_ref()
                .is_some_and(|now| Arc::ptr_eq(now, &token))
        );
        assert!(
            wait_until(WAIT, {
                let shared = h.shared.clone();
                let channel = h.channel_id;
                move || {
                    let shared = shared.clone();
                    Box::pin(
                        async move { shared.restart.deferred_hook_channels.contains_key(&channel) },
                    )
                }
            })
            .await,
            "production cancellation arms its backstop without a test kick"
        );
        held_fire(&h).await;
        assert_eq!(queued_ids(&h).await, vec![queued]);
        assert_eq!(h.provider_inputs().concat().matches(Q).count(), 0);
        token.bind_unmanaged_session_name(&tmux);
        assert_eq!(
            discord::zombie_foreground_release::tui_structurally_idle(&CLAUDE, &token),
            Some(true)
        );
        clear_row(&h, active);
        wake_existing(&h);
        answered_once(&h).await;
    });
}

/// A local pane probe over the existing streaming fixture's fake tmux protocol.
struct ProbePane {
    root: PathBuf,
    _path: crate::config::TestEnvVarGuard,
}

impl ProbePane {
    fn new(h: &RelayE2eHarness, tmux: &str) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let root = h.root.path().to_path_buf();
        let binary = root.join("tmux");
        let script = format!(
            r#"#!/bin/sh
if [ "$1" = -V ]; then echo 'tmux 3.5'; exit 0; fi
while [ "${{1#-}}" != "$1" ]; do shift; done
echo "$*" >> '{root}/tmux-calls'
case "$*" in *"{tmux}"*) read -r state < '{root}/pane' ;; *) state=dead ;; esac
case "$1" in
 has-session) [ "$state" = dead ] && {{ echo "can't find session" >&2; exit 1; }}; exit 0 ;;
 list-panes) [ "$state" = dead ] && echo 1 || echo 0; exit 0 ;;
 capture-pane) case "$state" in busy) printf '%s\n' '⏺ Running 1 shell command…' '· Actioning… (4m 7s · esc to interrupt)' ;; idle) cat '{root}/idle-pane' ;; esac; exit 0 ;;
 kill-session) echo "$*" >> '{root}/tmux-kills'; exit 0 ;;
 send-keys|load-buffer|paste-buffer) echo "$*" >> '{root}/tmux-writes'; exit 0 ;;
esac
exit 0
"#,
            root = root.display()
        );
        std::fs::write(&binary, script).unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = format!(
            "{}:{}",
            root.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let _path = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "PATH",
            Path::new(&path),
        );
        std::fs::write(root.join("pane"), "dead\n").unwrap();
        std::fs::write(root.join("idle-pane"), idle_pane("❯\u{a0}")).unwrap();
        Self { root, _path }
    }

    fn pane(&self, state: &str) {
        std::fs::write(self.root.join("pane.next"), format!("{state}\n")).unwrap();
        std::fs::rename(self.root.join("pane.next"), self.root.join("pane")).unwrap();
    }

    fn writes(&self) -> String {
        std::fs::read_to_string(self.root.join("tmux-writes")).unwrap_or_default()
    }
}

fn idle_pane(row: &str) -> String {
    let border = "─".repeat(60);
    format!(
        "⏺ Done.\n\n{border}\n{row}\n{border}\n  ⏵⏵ bypass permissions on (shift+tab to cycle)\n"
    )
}

/// The same real watcher loop and pane protocol used by streaming_harness_tests.
struct Watcher {
    _task: AbortOnDrop<()>,
    cancelled: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    heartbeat: Arc<std::sync::atomic::AtomicI64>,
    tmux: String,
    transcript: PathBuf,
    root: PathBuf,
    _path: crate::config::TestEnvVarGuard,
    _interval: crate::config::TestEnvVarGuard,
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

impl Watcher {
    async fn stop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        if let Some(task) = self._task.0.take() {
            task.abort();
            let _ = task.await;
        }
    }

    fn pane(&self, state: &str) {
        std::fs::write(self.root.join("pane.next"), format!("{state}\n")).unwrap();
        std::fs::rename(self.root.join("pane.next"), self.root.join("pane")).unwrap();
    }

    async fn pause(&self) {
        self.paused.store(true, Ordering::SeqCst);
        let at = discord::tmux_watcher_now_ms();
        assert!(
            wait_until(WAIT, {
                let heartbeat = self.heartbeat.clone();
                move || {
                    let heartbeat = heartbeat.clone();
                    Box::pin(async move { heartbeat.load(Ordering::Acquire) > at })
                }
            })
            .await,
            "the watcher observed its pause before pending bytes were appended"
        );
    }

    fn generation(&self) {
        let path = PathBuf::from(crate::services::tmux_common::session_temp_path(
            &self.tmux,
            "generation",
        ));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "cancel-reclaim-generation").unwrap();
    }

    fn append(&self, text: &str) {
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&self.transcript)
            .unwrap()
            .write_all(text.as_bytes())
            .unwrap();
    }

    fn attach(h: &RelayE2eHarness, transcript: PathBuf, tmux: String) -> Self {
        Self::attach_restoring(h, transcript, tmux, None, true)
    }

    fn attach_restoring(
        h: &RelayE2eHarness,
        transcript: PathBuf,
        tmux: String,
        restored: Option<&inflight::InflightTurnState>,
        with_generation: bool,
    ) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let root = h.root.path().to_path_buf();
        let binary = root.join("tmux");
        let script = format!(
            r#"#!/bin/sh
while [ "${{1#-}}" != "$1" ]; do shift; done
case "$*" in *"{tmux}"*) read -r state < '{root}/pane' ;; *) state=dead ;; esac
case "$1" in
 has-session) [ "$state" = dead ] && {{ echo "can't find session" >&2; exit 1; }}; exit 0 ;;
 list-panes) [ "$state" = dead ] && echo 1 || echo 0; exit 0 ;;
 capture-pane) case "$state" in busy) printf '%s\n' '⏺ Running 1 shell command…' '· Actioning… (4m 7s · esc to interrupt)' ;; idle) cat '{root}/idle-pane' ;; esac; exit 0 ;;
 kill-session) echo "$*" >> '{root}/tmux-kills'; exit 0 ;;
esac
exit 0
"#,
            root = root.display()
        );
        std::fs::write(&binary, script).unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = format!(
            "{}:{}",
            root.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let path_guard = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "PATH",
            Path::new(&path),
        );
        let interval = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_STATUS_INTERVAL_SECS",
            Path::new("0"),
        );
        std::fs::write(root.join("pane"), "busy\n").unwrap();
        std::fs::write(root.join("idle-pane"), idle_pane("❯\u{a0}")).unwrap();
        let (cancelled, paused, resume_offset, pause_epoch, turn_delivered, heartbeat) = {
            let handle = h
                .shared
                .tmux_watchers
                .get(&h.channel_id)
                .expect("attached watcher");
            (
                handle.cancel.clone(),
                handle.paused.clone(),
                handle.resume_offset.clone(),
                handle.pause_epoch.clone(),
                handle.turn_delivered.clone(),
                handle.last_heartbeat_ts_ms.clone(),
            )
        };
        let restored = restored
            .and_then(|row| discord::tmux::restored_watcher_turn_from_inflight(row, &tmux, false));
        let task = tokio::spawn(discord::tmux::tmux_output_watcher_with_restore(
            h.channel_id,
            h.ctx.http.clone(),
            h.shared.clone(),
            transcript.display().to_string(),
            tmux.clone(),
            0,
            cancelled.clone(),
            paused.clone(),
            resume_offset,
            pause_epoch,
            turn_delivered,
            heartbeat.clone(),
            restored,
        ));
        let watcher = Self {
            _task: AbortOnDrop(Some(task)),
            cancelled,
            paused,
            heartbeat,
            tmux,
            transcript,
            root,
            _path: path_guard,
            _interval: interval,
        };
        watcher.pane("busy");
        if with_generation {
            watcher.generation();
        }
        watcher
    }
}

fn user(text: &str) -> String {
    format!(
        "{}\n",
        serde_json::json!({"type":"user","message":{"role":"user","content":text}})
    )
}

fn said(text: &str) -> String {
    format!(
        "{}\n",
        serde_json::json!({"type":"assistant","message":{"role":"assistant",
        "content":[{"type":"text","text":text}]}})
    )
}

fn result() -> String {
    format!(
        "{}\n",
        serde_json::json!({"type":"result","subtype":"success","result":"",
        "session_id":super::SESSION_UUID})
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn released_anchor_preserves_prior_tail_and_runs_q_exactly_once() {
    if !isolated(concat!(
        module_path!(),
        "::released_anchor_preserves_prior_tail_and_runs_q_exactly_once"
    )) {
        return;
    }
    let h = harness().await;
    let base = recent_snowflake_base();
    let (active, queued, stop) = (base | 11, base | 12, base | 13);
    let (held, token) = hold_a_turn(&h, active).await;
    let tmux = CLAUDE.build_tmux_session_name("i6016-prior-tail");
    let transcript = h.attach_tmux_watcher(&tmux, "cancel-reclaim-prior.jsonl");
    h.answer_placeholders_immediately();
    let anchor = h.channel_id.say(&h.ctx.http, "...").await.unwrap().id;
    let mut row = seed_row(&h, active, &token, None, Some(&transcript));
    row.current_msg_id = anchor.get();
    row.set_relay_owner_kind(inflight::RelayOwnerKind::Watcher);
    row.runtime_kind = Some(crate::services::agent_protocol::RuntimeHandoffKind::LegacyTmuxWrapper);
    fixture_readiness(&h, &tmux, row.runtime_kind.unwrap(), None);
    write_row(&row);
    // Legacy attachment has no wrapper generation, so stop cannot suppress another owner's range.
    let watcher = Watcher::attach_restoring(&h, transcript, tmux, Some(&row), false);
    watcher.append(&format!("{}{}", user("previous turn"), said(HEAD)));
    let head_visible = wait_until(WAIT, {
        let messages = h.mock.messages.clone();
        move || {
            let messages = messages.clone();
            Box::pin(async move {
                messages
                    .lock()
                    .unwrap()
                    .values()
                    .any(|(_, text)| text.contains(HEAD))
            })
        }
    })
    .await;
    assert!(
        head_visible,
        "the real watcher delivered A's head before cancellation: {:?}",
        h.messages()
    );
    watcher.pause().await;
    let head_before = h.messages();
    assert_eq!(
        head_before
            .iter()
            .filter(|(_, content)| content.contains(HEAD))
            .count(),
        1,
        "A's head occupies one delivered message before cancellation"
    );
    let head_occurrences_before = head_before
        .iter()
        .map(|(_, content)| content.matches(HEAD).count())
        .sum::<usize>();
    watcher.append(&said(TAIL));
    assert_eq!(
        std::fs::read_to_string(&watcher.transcript)
            .unwrap()
            .matches(TAIL)
            .count(),
        1,
        "A's pending tail already exists on disk at the cancellation boundary"
    );
    assert!(!h.messages().iter().any(|(_, text)| text.contains(TAIL)));
    h.deliver_user_message(queued, Q).await.unwrap();
    stop_and_abort(&h, &token, held, stop).await;
    assert!(
        token.tmux_session_name().is_none(),
        "the stopped placeholder-held intake had no pane binding"
    );
    assert!(!h.messages().iter().any(|(_, text)| text.contains(TAIL)));
    row.tmux_session_name = Some(watcher.tmux.clone());
    write_row(&row);
    watcher.generation();
    token.bind_unmanaged_session_name(&watcher.tmux);
    held_fire(&h).await;

    // Let the listener finish its initial reconcile while A still owns the slot.
    let reconciles_before =
        queue_io::cancel_backstop_test_support::listener_completed_reconciles(h.channel_id);
    discord::spawn_turn_completion_idle_queue_listener(h.shared.clone(), CLAUDE);
    assert!(
        wait_until(WAIT, {
            let channel = h.channel_id;
            move || {
                Box::pin(async move {
                    queue_io::cancel_backstop_test_support::listener_completed_reconciles(channel)
                        > reconciles_before
                })
            }
        })
        .await,
        "listener initial reconcile completed before the race is installed"
    );
    let race = discord::queue_dispatch::kickoff::cancel_backstop_test_support::DequeueRace::install(
        h.channel_id,
    );
    watcher.pane("idle");
    clear_row(&h, active);
    wake_existing(&h);
    let arrived = race.wait_for_both(WAIT).await;
    assert!(
        arrived.contains(&"turn_completion_event") && arrived.contains(&"idle_queue_backstop"),
        "two independent production consumers reached Q before dequeue: {arrived:?}"
    );
    assert_eq!(queued_ids(&h).await, vec![queued]);
    assert!(
        h.provider_inputs().is_empty(),
        "both contenders are before actual dequeue"
    );
    let claims_before = h.mailbox().await.claim_observation;
    race.release();
    answered_once(&h).await;
    let finished = race.wait_for_both_finished(WAIT).await;
    assert!(
        finished.contains(&"turn_completion_event") && finished.contains(&"idle_queue_backstop"),
        "both actual kickoff consumers returned before final counts: {finished:?}"
    );
    let claims_after = h.mailbox().await.claim_observation;
    match (claims_before, claims_after) {
        (
            crate::services::turn_orchestrator::ClaimObservation::Actor {
                incarnation: before_actor,
                claim_seq: before_seq,
            },
            crate::services::turn_orchestrator::ClaimObservation::Actor {
                incarnation: after_actor,
                claim_seq: after_seq,
            },
        ) => {
            assert_eq!(
                before_actor, after_actor,
                "the same real actor arbitrated both consumers"
            );
            assert_eq!(
                after_seq - before_seq,
                1,
                "Q acquired exactly one actual actor claim"
            );
        }
        evidence => panic!("actual actor claim observations required: {evidence:?}"),
    }

    assert!(
        !h.messages().iter().any(|(_, text)| text.contains(TAIL)),
        "the pending tail stayed unread through Q's completion"
    );
    watcher.append(&result());
    watcher.paused.store(false, Ordering::Release);
    assert!(
        wait_until(WAIT, {
            let messages = h.mock.messages.clone();
            move || {
                let messages = messages.clone();
                Box::pin(async move {
                    messages
                        .lock()
                        .unwrap()
                        .values()
                        .any(|(_, text)| text.contains(TAIL))
                })
            }
        })
        .await,
        "A's late terminal must deliver its pending tail: {:?}",
        h.messages()
    );
    let end = std::fs::metadata(&watcher.transcript).unwrap().len();
    assert!(
        wait_until(WAIT, {
            let shared = h.shared.clone();
            let channel = h.channel_id;
            move || {
                let shared = shared.clone();
                Box::pin(async move {
                    shared
                        .tmux_relay_coord(channel)
                        .confirmed_end_offset
                        .load(Ordering::Acquire)
                        >= end
                })
            }
        })
        .await,
        "the late terminal completed its delivery frontier"
    );
    assert_eq!(
        h.messages()
            .iter()
            .filter(|(_, content)| content.contains(HEAD))
            .count(),
        1,
        "A's late terminal did not deliver a second head message"
    );
    assert_eq!(
        h.messages()
            .iter()
            .map(|(_, content)| content.matches(HEAD).count())
            .sum::<usize>(),
        head_occurrences_before,
        "A's pre-existing body and spinner preview gained no additional head copy"
    );
    assert_eq!(
        h.messages()
            .iter()
            .map(|(_, content)| content.matches(TAIL).count())
            .sum::<usize>(),
        1,
        "A's previously unread tail is delivered exactly once: {:?}",
        h.messages()
    );
    answered_once(&h).await;
    assert!(
        !std::fs::read_to_string(watcher.root.join("tmux-kills"))
            .unwrap_or_default()
            .contains(&watcher.tmux),
        "queue recovery did not kill A's watcher session"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reclaimable_foreign_row_is_swept_then_backstop_runs_q_once() {
    if !isolated(concat!(
        module_path!(),
        "::reclaimable_foreign_row_is_swept_then_backstop_runs_q_once"
    )) {
        return;
    }
    let h = harness().await;
    let base = recent_snowflake_base();
    let (active, queued, stop) = (base | 21, base | 22, base | 23);
    let (held, token) = hold_a_turn(&h, active).await;
    let tmux = CLAUDE.build_tmux_session_name("i6016-dead-foreign");
    let watcher = ProbePane::new(&h, &tmux);
    let transcript = h.root.path().join("cancel-backstop-foreign.jsonl");
    watcher.pane("dead");
    std::fs::write(&transcript, user("previous FOREIGN turn") + &result()).unwrap();
    fixture_readiness(
        &h,
        &tmux,
        crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
        Some(&transcript),
    );
    let mut row = seed_row(&h, active, &token, Some(&tmux), Some(&transcript));
    row.set_relay_owner_kind(inflight::RelayOwnerKind::Watcher);
    row.runtime_kind = Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui);
    let stale = (chrono::Local::now() - chrono::Duration::seconds(121))
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();
    row.started_at = stale.clone();
    row.updated_at = stale;
    write_row(&row);
    assert_eq!(
        crate::services::tui_turn_state::jsonl_ready_for_input(
            &CLAUDE,
            row.runtime_kind,
            &transcript,
            Some(row.last_offset),
        ),
        Some(crate::services::tui_turn_state::TuiReadyState::Ready)
    );
    h.deliver_user_message(queued, Q).await.unwrap();
    stop_and_abort(&h, &token, held, stop).await;
    held_fire(&h).await;
    let registry = h.health_registry.as_ref().unwrap();
    registry
        .register("claude".to_string(), h.shared.clone())
        .await;
    assert_eq!(
        discord::relay_recovery::leaked_row_sweep::sweep_leaked_inflight_rows(registry, &CLAUDE)
            .await,
        1,
        "existing FOREIGN sweep used terminal Ready, halted capture and episode identity"
    );
    assert!(inflight::load_inflight_state_read_only(&CLAUDE, h.channel_id.get()).is_none());
    if h.shared
        .restart
        .deferred_hook_channels
        .contains_key(&h.channel_id)
    {
        wake_existing(&h);
    }
    answered_once(&h).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_foreign_row_survives_sweep_and_backstop() {
    if !isolated(concat!(
        module_path!(),
        "::live_foreign_row_survives_sweep_and_backstop"
    )) {
        return;
    }
    let h = harness().await;
    let base = recent_snowflake_base();
    let (active, queued, stop) = (base | 31, base | 32, base | 33);
    let (held, token) = hold_a_turn(&h, active).await;
    let tmux = CLAUDE.build_tmux_session_name("i6016-live-foreign");
    let watcher = ProbePane::new(&h, &tmux);
    let transcript = h.root.path().join("cancel-backstop-foreign.jsonl");
    watcher.pane("busy");
    std::fs::write(&transcript, user("previous FOREIGN turn")).unwrap();
    fixture_readiness(
        &h,
        &tmux,
        crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
        Some(&transcript),
    );
    let mut row = seed_row(&h, active, &token, Some(&tmux), Some(&transcript));
    row.set_relay_owner_kind(inflight::RelayOwnerKind::Watcher);
    row.runtime_kind = Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui);
    let stale = (chrono::Local::now() - chrono::Duration::seconds(121))
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();
    row.started_at = stale.clone();
    row.updated_at = stale;
    write_row(&row);
    assert_eq!(
        crate::services::tui_turn_state::jsonl_ready_for_input(
            &CLAUDE,
            row.runtime_kind,
            &transcript,
            Some(row.last_offset),
        ),
        Some(crate::services::tui_turn_state::TuiReadyState::Busy)
    );
    h.deliver_user_message(queued, Q).await.unwrap();
    stop_and_abort(&h, &token, held, stop).await;
    held_fire(&h).await;
    let registry = h.health_registry.as_ref().unwrap();
    registry
        .register("claude".to_string(), h.shared.clone())
        .await;
    let before = inflight::load_inflight_state_read_only(&CLAUDE, h.channel_id.get()).unwrap();
    assert_eq!(
        discord::relay_recovery::leaked_row_sweep::sweep_leaked_inflight_rows(registry, &CLAUDE)
            .await,
        0
    );
    held_fire(&h).await;
    let after = inflight::load_inflight_state_read_only(&CLAUDE, h.channel_id.get()).unwrap();
    assert_eq!(
        inflight::InflightTurnIdentity::from_state(&before),
        inflight::InflightTurnIdentity::from_state(&after)
    );
    assert_eq!(
        after.effective_relay_owner_kind(),
        inflight::RelayOwnerKind::Watcher
    );
    assert!(
        h.mailbox()
            .await
            .cancel_token
            .as_ref()
            .is_some_and(|now| Arc::ptr_eq(now, &token))
    );
    assert_eq!(queued_ids(&h).await, vec![queued]);
    assert!(
        h.provider_inputs().is_empty(),
        "a live foreign row never loses its anchor"
    );

    let snapshot = h.mailbox().await;
    h.shared
        .queue_park_ledger
        .evaluate(&h.shared, &CLAUDE, h.channel_id, &snapshot);
    h.shared
        .queue_park_ledger
        .age_sources_for_tests(h.channel_id, Duration::from_secs(600));
    #[derive(Clone)]
    struct Capture(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer({
            let bytes = bytes.clone();
            move || Capture(bytes.clone())
        })
        .finish();
    crate::logging::test_capture::pin_callsite_interest();
    tracing::subscriber::with_default(subscriber, || {
        tracing::error!(target: "agentdesk::discord::queue_park", "positive capture control");
        for _ in 0..2 {
            h.shared
                .queue_park_ledger
                .evaluate(&h.shared, &CLAUDE, h.channel_id, &snapshot);
        }
    });
    let captured = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
    assert!(captured.contains("positive capture control"));
    assert_eq!(
        captured
            .matches("cancel-preserved source remains parked")
            .count(),
        1,
        "{captured}"
    );
    let park = h
        .shared
        .queue_park_ledger
        .project(&h.shared, &CLAUDE, h.channel_id, &snapshot);
    assert_eq!(park.owner, Some("idle_queue_backstop"));
    assert_eq!(
        park.reason.as_deref(),
        Some("cancelled_anchor_held:hold_inflight_present")
    );
    let detail =
        serde_json::to_value(discord::health::build_health_snapshot(registry).await).unwrap();
    let health = detail["mailboxes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|mailbox| mailbox["channel_id"].as_u64() == Some(h.channel_id.get()))
        .expect("real health mailbox projection");
    assert_eq!(health["queue_park_owner"], "idle_queue_backstop");
    assert_eq!(
        health["queue_park_reason"],
        "cancelled_anchor_held:hold_inflight_present"
    );
    assert_eq!(
        health["queue_park_tracked_source_ids"],
        serde_json::json!([queued])
    );

    // Positive control supplies native terminal evidence after proving the live owner was held.
    use std::io::Write;
    std::fs::OpenOptions::new()
        .append(true)
        .open(&transcript)
        .unwrap()
        .write_all(result().as_bytes())
        .unwrap();
    watcher.pane("idle");
    token.bind_unmanaged_session_name(&tmux);
    assert_eq!(
        discord::zombie_foreground_release::tui_structurally_idle(&CLAUDE, &token),
        Some(true)
    );
    clear_row(&h, active);
    wake_existing(&h);
    answered_once(&h).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_bound_row_keeps_its_anchor_despite_dead_pane() {
    if !isolated(concat!(
        module_path!(),
        "::session_bound_row_keeps_its_anchor_despite_dead_pane"
    )) {
        return;
    }
    let h = harness().await;
    let base = recent_snowflake_base();
    let (active, queued, stop) = (base | 51, base | 52, base | 53);
    let (held, token) = hold_a_turn(&h, active).await;
    let tmux = CLAUDE.build_tmux_session_name("i6016-session-bound");
    let pane = ProbePane::new(&h, &tmux);
    pane.pane("dead");
    let transcript = h.root.path().join("session-bound.jsonl");
    std::fs::write(&transcript, user("previous FOREIGN turn")).unwrap();
    let mut row = seed_row(&h, active, &token, Some(&tmux), Some(&transcript));
    row.runtime_kind = Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui);
    row.set_relay_owner_kind(inflight::RelayOwnerKind::SessionBoundRelay);
    let stale = (chrono::Local::now() - chrono::Duration::seconds(121))
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();
    row.started_at = stale.clone();
    row.updated_at = stale;
    write_row(&row);
    h.deliver_user_message(queued, Q).await.unwrap();
    stop_and_abort(&h, &token, held, stop).await;
    held_fire(&h).await;
    let before = inflight::load_inflight_state_read_only(&CLAUDE, h.channel_id.get()).unwrap();
    assert_eq!(
        discord::relay_recovery::leaked_row_sweep::sweep_leaked_inflight_rows(
            h.health_registry.as_ref().unwrap(),
            &CLAUDE
        )
        .await,
        0
    );
    held_fire(&h).await;
    let after = inflight::load_inflight_state_read_only(&CLAUDE, h.channel_id.get()).unwrap();
    assert_eq!(
        inflight::InflightTurnIdentity::from_state(&before),
        inflight::InflightTurnIdentity::from_state(&after)
    );
    assert_eq!(
        after.effective_relay_owner_kind(),
        inflight::RelayOwnerKind::SessionBoundRelay
    );
    assert!(
        h.mailbox()
            .await
            .cancel_token
            .as_ref()
            .is_some_and(|now| Arc::ptr_eq(now, &token))
    );
    assert_eq!(queued_ids(&h).await, vec![queued]);
    assert!(h.provider_inputs().is_empty());
    clear_row(&h, active);
    wake_existing(&h);
    answered_once(&h).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelled_anchor_recovery_keeps_a_person_draft_and_q_until_lift_pg() {
    if !isolated(concat!(
        module_path!(),
        "::cancelled_anchor_recovery_keeps_a_person_draft_and_q_until_lift_pg"
    )) {
        return;
    }
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use crate::services::agent_protocol::RuntimeHandoffKind;
    use crate::services::claude_tui::{composer_lock, host_input::FakeDraftPane};
    let mut database = None;
    let h = RelayE2eHarness::start_bound_on(async {
        let fixture = TestPostgresDb::create().await;
        let pool = fixture.connect_and_migrate().await;
        database = Some(fixture);
        pool
    })
    .await;
    let database = database.expect("an isolated mandatory PostgreSQL fixture");
    let pool = h.shared.pg_pool.clone().expect("actual PG-backed runtime");
    let (database_name, backend_pid): (String, i32) =
        sqlx::query_as("SELECT current_database(), pg_backend_pid()")
            .fetch_one(&pool)
            .await
            .expect("actual PostgreSQL witness query");
    assert!(database_name.starts_with("agentdesk_db_auto_queue_"));
    eprintln!("I6016_HUMAN_PG database={database_name} backend_pid={backend_pid}");
    h.register_channel_in_role_map();
    h.cache_relay_transport();
    h.shared
        .restart
        .reconcile_done
        .store(true, Ordering::SeqCst);
    let base = recent_snowflake_base();
    let (active, queued, stop) = (base | 61, base | 62, base | 63);
    let (held, token) = hold_a_turn(&h, active).await;
    seed_row(&h, active, &token, None, None);
    h.deliver_user_message(queued, Q).await.unwrap();
    stop_and_abort(&h, &token, held, stop).await;
    held_fire(&h).await;
    let name = "i6016-person-draft";
    let tmux = CLAUDE.build_tmux_session_name(name);
    let pane = ProbePane::new(&h, &tmux);
    pane.pane("idle");
    let home = h.root.path().join("claude-home");
    let _home = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "CLAUDE_CONFIG_DIR",
        &home,
    );
    let transcript = crate::services::claude_tui::transcript_tail::claude_transcript_path(
        h.root.path(),
        super::SESSION_UUID,
        Some(&home),
    )
    .unwrap();
    std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    std::fs::write(&transcript, format!("{}\n", serde_json::json!({"type":"system","subtype":"turn_duration","sessionId":super::SESSION_UUID}))).unwrap();
    fixture_readiness(&h, &tmux, RuntimeHandoffKind::ClaudeTui, Some(&transcript));
    crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
        &tmux,
        crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            output_path: transcript.display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: Some(super::SESSION_UUID.to_string()),
            last_offset: 0,
            relay_last_offset: None,
        },
    );
    {
        let mut core = h.shared.core.lock().await;
        core.sessions.get_mut(&h.channel_id).unwrap().channel_name = Some(name.to_string());
    }
    let _hook = crate::services::claude_tui::hook_server::publish_hook_endpoint(format!(
        "http://{}/test-hook",
        h.proxy
    ));
    crate::services::provider_hosting::install_provider_hosting_config(
        &crate::config::Config::default(),
    );
    let draft = FakeDraftPane::new(&tmux);
    let person = "\x1b[39m❯\u{a0}person typed this without Enter";
    let original = idle_pane(person);
    draft.show(&original);
    // The regular readiness capture is empty; the attributed final read sees the person's text.
    token.bind_unmanaged_session_name(&tmux);
    assert_eq!(
        discord::zombie_foreground_release::tui_structurally_idle(&CLAUDE, &token),
        Some(true)
    );
    clear_row(&h, active);
    assert!(
        !discord::router::hosted_tui_promote_readiness_blocked(&h.shared, &CLAUDE, h.channel_id)
            .await,
        "real host evidence and native Idle must permit the final composer read"
    );
    wake_existing(&h);
    assert!(
        wait_until(WAIT, {
            let shared = h.shared.clone();
            let channel = h.channel_id;
            let tmux = tmux.clone();
            move || {
                let shared = shared.clone();
                let tmux = tmux.clone();
                Box::pin(async move {
                    let snapshot = mailbox_snapshot(&shared, channel).await;
                    composer_lock::draft_guarded(&tmux)
                        && snapshot.cancel_token.is_none()
                        && snapshot.intervention_queue.iter().any(|i| i.text == Q)
                })
            }
        })
        .await,
        "the final composer read must refuse and restore Q: draft_reads={}; cancel_present={}; queue={:?}; inputs={:?}; messages={:?}; writes={}",
        draft.draft_reads(),
        h.mailbox().await.cancel_token.is_some(),
        queued_ids(&h).await,
        h.provider_inputs(),
        h.messages(),
        pane.writes()
    );
    assert!(
        draft.draft_reads() >= 1,
        "the person's actual attributed composer was read"
    );
    assert_eq!(
        crate::services::claude_tui::host_input::observe_draft(&tmux),
        Some(original),
        "the person's attributed composer bytes remain unchanged"
    );
    assert!(
        pane.writes().is_empty(),
        "draft protection sends no cleanup, paste or Enter: {}",
        pane.writes()
    );
    assert!(
        !h.root.path().join(super::PROVIDER_INPUTS_FILE).exists(),
        "no fallback CLI execution may bypass the draft"
    );
    assert_eq!(queued_ids(&h).await, vec![queued]);
    assert_eq!(h.durable_queue()[0].text, Q);
    assert!(composer_lock::draft_guarded(&tmux));
    // Existing main's draft-lift owner performs the retry; only the fixture pane changes.
    let empty = "\x1b[39m❯\u{a0}\x1b[2mTry \"refactor <filepath>\"\x1b[0m";
    let script_path = pane.root.join("tmux");
    let mut script = std::fs::read_to_string(&script_path).unwrap();
    let stream = pane.root.join("tui-done.jsonl");
    let done = format!(
        "{}\n",
        serde_json::json!({"type":"result","subtype":"success","result":"ok","session_id":super::SESSION_UUID})
    );
    std::fs::write(&stream, format!("{}{}", said("ok"), done)).unwrap();
    script=script.replace("send-keys|load-buffer|paste-buffer) echo \"$*\" >>", "send-keys|load-buffer|paste-buffer) case \" $* \" in *\" Enter \"*) cat 'STREAM' >> 'TRANSCRIPT' ;; esac; echo \"$*\" >>")
        .replace("STREAM", &stream.display().to_string()).replace("TRANSCRIPT", &transcript.display().to_string());
    std::fs::write(&script_path, script).unwrap();
    draft.show(&idle_pane(empty));
    assert!(
        wait_until(WAIT, {
            let shared = h.shared.clone();
            let channel = h.channel_id;
            let messages = h.mock.messages.clone();
            move || {
                let shared = shared.clone();
                let messages = messages.clone();
                Box::pin(async move {
                    let snapshot = mailbox_snapshot(&shared, channel).await;
                    snapshot.cancel_token.is_none()
                        && snapshot.intervention_queue.is_empty()
                        && messages
                            .lock()
                            .unwrap()
                            .values()
                            .any(|(_, text)| text == "ok")
                })
            }
        })
        .await,
        "main draft-lift retry must actually submit and answer Q: {:?}; writes={}",
        h.messages(),
        pane.writes()
    );
    let writes = pane.writes();
    assert_eq!(
        writes.matches(Q).count(),
        1,
        "one actual literal/paste input: {writes}"
    );
    assert_eq!(
        writes
            .lines()
            .filter(|line| line.split_whitespace().any(|word| word == "Enter"))
            .count(),
        1,
        "one actual Enter: {writes}"
    );
    assert_eq!(
        h.messages().iter().filter(|(_, text)| text == "ok").count(),
        1
    );
    assert!(h.durable_queue().is_empty());
    assert!(
        h.unhandled_requests().is_empty(),
        "{:?}",
        h.unhandled_requests()
    );
    pool.close().await;
    database.drop().await;
}

#[test]
fn restart_restores_q_and_inflight_then_runs_q_once() {
    if !isolated(concat!(
        module_path!(),
        "::restart_restores_q_and_inflight_then_runs_q_once"
    )) {
        return;
    }
    run_with_large_stack(|| async {
        let mut h = harness().await;
        let base = recent_snowflake_base();
        let (active, queued) = (base | 41, base | 42);
        let (held, token) = hold_a_turn(&h, active).await;
        h.deliver_user_message(queued, Q).await.unwrap();
        assert_eq!(queued_ids(&h).await, vec![queued]);
        assert_eq!(h.durable_queue().len(), 1, "Q was persisted before restart");
        abort_held_intake(held).await;
        h.answer_placeholders_immediately();
        // Legacy row has no channel-name binding; recovery preserves the direct-provider route for Q.
        let tmux = "cancel-backstop-restored-fixture".to_string();
        let transcript = h.attach_tmux_watcher(&tmux, "cancel-reclaim-restored.jsonl");
        let mut watcher = Watcher::attach(&h, transcript.clone(), tmux.clone());
        watcher.stop().await;
        h.shared.tmux_watchers.remove(&h.channel_id);
        watcher.pane("busy");
        std::fs::write(&transcript, user("restored turn A")).unwrap();
        let anchor = h.channel_id.say(&h.ctx.http, "...").await.unwrap().id;
        let mut row = seed_row(&h, active, &token, Some(&tmux), Some(&transcript));
        row.current_msg_id = anchor.get();
        row.runtime_kind = Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui);
        row.set_relay_owner_kind(inflight::RelayOwnerKind::Watcher);
        row.born_generation = 0;
        row.worktree_path = Some(h.root.path().display().to_string());
        row.session_id = Some(super::SESSION_UUID.to_string());
        write_row(&row);
        let root = discord::runtime_store::discord_inflight_root().unwrap();
        let row_path = inflight::inflight_state_path(&root, &CLAUDE, h.channel_id.get());
        let mut wire = serde_json::to_value(&row).unwrap();
        for field in ["turn_nonce", "relay_owner_kind", "restart_generation"] {
            wire.as_object_mut().unwrap().remove(field);
        }
        std::fs::write(&row_path, serde_json::to_vec_pretty(&wire).unwrap()).unwrap();

        // Keep the root and Discord transport, while replacing every in-memory mailbox owner.
        let shared = discord::make_shared_data_for_tests();
        {
            let mut settings = shared.settings.write().await;
            settings.owner_user_id = Some(USER_ID);
            settings.allow_all_users = true;
        }
        shared.http.cached_serenity_ctx.set(h.ctx.clone()).unwrap();
        shared
            .http
            .cached_bot_token
            .set(h.data.token.clone())
            .unwrap();
        h.shared = shared.clone();
        h.data.shared = shared.clone();
        let mut completions = h.subscribe_completions();
        assert!(
            discord::restart_report::load_restart_report(&CLAUDE, h.channel_id.get()).is_none(),
            "legacy fixture exercises ordinary inflight restore without a restart report"
        );
        let cards = discord::runtime_bootstrap::restore_queued_and_inflight_for_tests(
            &h.ctx.http,
            &shared,
            &CLAUDE,
        )
        .await;
        assert!(cards.is_empty(), "the restored queue has no stale cards");
        shared.restart.reconcile_done.store(true, Ordering::SeqCst);
        let restored = h.mailbox().await;
        assert_eq!(
            restored.active_user_message_id.map(|id| id.get()),
            Some(active)
        );
        assert!(
            restored.cancel_token.is_some(),
            "legacy inflight recovered foreground ownership"
        );
        assert_eq!(
            shared
                .tmux_watchers
                .get(&h.channel_id)
                .unwrap()
                .tmux_session_name,
            tmux,
            "normal restore preserves the fixture's legacy raw tmux identity"
        );
        assert!(
            shared.core.lock().await.sessions[&h.channel_id]
                .channel_name
                .is_none(),
            "ordinary legacy restore retains Q's direct-provider route"
        );
        assert_eq!(queued_ids(&h).await, vec![queued]);
        assert!(
            h.provider_inputs().is_empty(),
            "Q cannot launch before restored A ends"
        );
        discord::spawn_turn_completion_idle_queue_listener(shared.clone(), CLAUDE);
        watcher.append(&format!(
            "{}{}",
            said("restored turn A final response"),
            result()
        ));
        watcher.pane("idle");
        let owner = tokio::time::timeout(WAIT, async {
            loop {
                let event = completions.recv().await.unwrap();
                if event.queue_is_eligible() && event.turn_id == Some(active) {
                    break event;
                }
            }
        })
        .await
        .expect("restored turn's completion event owns queue eligibility");
        assert_eq!(owner.channel_id, h.channel_id);
        answered_once(&h).await;
        let restored_identity = shared.tmux_watchers.get(&h.channel_id).map(|watcher| {
            (
                watcher.tmux_session_name.clone(),
                watcher.output_path.clone(),
                watcher.cancel.clone(),
            )
        });
        if let Some((session, output, cancel)) = restored_identity {
            shared.tmux_watchers.cancel_and_remove_channel_if_current(
                &h.channel_id,
                &session,
                &output,
                &cancel,
            );
        }
    });
}

/// The actual CLI stub records stdin, then waits on this fixture-owned FIFO before replying.
struct ProviderPause {
    fifo: PathBuf,
    entered: PathBuf,
}

impl ProviderPause {
    fn install(h: &RelayE2eHarness) -> Self {
        let fifo = h.root.path().join("provider-q-release.fifo");
        let entered = h.root.path().join("provider-q-received");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        let stub = h.root.path().join("claude-stub");
        let script = std::fs::read_to_string(&stub).unwrap();
        let boundary = "echo '{\"type\":\"system\",\"subtype\":\"init\"";
        assert_eq!(script.matches(boundary).count(), 1);
        let gate = format!(
            "printf ready > '{}'\nread -r released < '{}'\n{boundary}",
            entered.display(),
            fifo.display()
        );
        std::fs::write(&stub, script.replace(boundary, &gate)).unwrap();
        Self { fifo, entered }
    }

    fn write_release(path: &Path) -> bool {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
            .is_ok_and(|mut fifo| fifo.write_all(b"release\n").is_ok())
    }

    async fn release(&self) {
        let fifo = self.fifo.clone();
        assert!(
            wait_until(WAIT, move || {
                let fifo = fifo.clone();
                Box::pin(async move { Self::write_release(&fifo) })
            })
            .await,
            "the actual waiting provider accepted its fixture release"
        );
    }
}

impl Drop for ProviderPause {
    fn drop(&mut self) {
        let _ = Self::write_release(&self.fifo);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn active_q_reinsert_and_real_kick_leave_one_provider_input_and_answer() {
    if !isolated(concat!(
        module_path!(),
        "::active_q_reinsert_and_real_kick_leave_one_provider_input_and_answer"
    )) {
        return;
    }
    let h = harness().await;
    let base = recent_snowflake_base();
    let (active, queued, stop) = (base | 71, base | 72, base | 73);
    let (held, token) = hold_a_turn(&h, active).await;
    seed_row(&h, active, &token, None, None);
    h.deliver_user_message(queued, Q).await.unwrap();
    let source = h.mailbox().await.intervention_queue[0].clone();
    assert_eq!(source.message_id.get(), queued);
    assert_eq!(source.text, Q);
    stop_and_abort(&h, &token, held, stop).await;
    held_fire(&h).await;
    let tmux = CLAUDE.build_tmux_session_name("i6016-dedup-control");
    let pane = ProbePane::new(&h, &tmux);
    pane.pane("dead");
    token.bind_unmanaged_session_name(&tmux);
    clear_row(&h, active);
    let provider = ProviderPause::install(&h);
    wake_existing(&h);
    assert!(
        wait_until(WAIT, {
            let shared = h.shared.clone();
            let channel = h.channel_id;
            let entered = provider.entered.clone();
            let inputs = h.root.path().join(super::PROVIDER_INPUTS_FILE);
            move || {
                let shared = shared.clone();
                let entered = entered.clone();
                let inputs = inputs.clone();
                Box::pin(async move {
                    let snapshot = mailbox_snapshot(&shared, channel).await;
                    snapshot
                        .active_user_message_id
                        .is_some_and(|id| id.get() == queued)
                        && snapshot.cancel_token.is_some()
                        && entered.is_file()
                        && inputs.is_file()
                        && std::fs::read_to_string(inputs).unwrap().matches(Q).count() == 1
                })
            }
        })
        .await,
        "Q was actually claimed and received by the still-paused CLI stub"
    );
    let claimed = h.mailbox().await.claim_observation;
    let reinsert =
        discord::mailbox_enqueue_intervention(&h.shared, &CLAUDE, h.channel_id, source).await;
    assert!(!reinsert.enqueued);
    assert_eq!(
        reinsert.refusal_reason,
        Some(crate::services::turn_orchestrator::EnqueueRefusalReason::AlreadyActiveTurn)
    );
    let kicked = discord::queue_dispatch::kickoff::kickoff_idle_queue_channel(
        &discord::router::IntakeDeps {
            http: &h.ctx.http,
            cache: Some(&h.ctx.cache),
            ctx_for_chained_dispatch: Some(&h.ctx),
            shared: &h.shared,
            token: &h.data.token,
        },
        &CLAUDE,
        h.channel_id,
    )
    .await;
    assert!(
        !kicked.started,
        "a real duplicate kickoff cannot claim the running Q"
    );
    assert_eq!(h.mailbox().await.claim_observation, claimed);
    assert!(h.mailbox().await.intervention_queue.is_empty());
    assert_eq!(h.provider_inputs().concat().matches(Q).count(), 1);
    assert!(
        !h.messages().iter().any(|(_, text)| text == "ok"),
        "Q's actual provider remains blocked before its reply"
    );
    provider.release().await;
    answered_once(&h).await;
    assert_eq!(h.provider_starts(), 1);
}
