//! Producer revival through the supervised watcher and periodic health entry points.

use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::time::Duration;

use poise::serenity_prelude::{ChannelId, MessageId, UserId};
use serde_json::{Value, json};

use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::cluster::session_matcher::MatchedChannel;
use crate::services::discord::health::HealthRegistry;
use crate::services::discord::health::test_support::{MockDiscord, message_json};
use crate::services::discord::inflight::{self, InflightTurnState, RelayOwnerKind};
use crate::services::discord::outbound::{completed_turn_ledger, delivery_record};
use crate::services::discord::{self, SharedData, TmuxWatcherHandle};
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::tui_prompt_dedupe::{self as dedupe, TuiRuntimeBinding};

const OWNER: u64 = 343_742_347_365_974_026;
const SESSION: &str = "resume-entry-session";
const NATIVE_TURN: &str = "resume-entry-native-turn";
const PROMPT: &str = "continue this exact managed turn";

fn records(values: impl IntoIterator<Item = Value>) -> Vec<u8> {
    values
        .into_iter()
        .flat_map(|v| format!("{v}\n").into_bytes())
        .collect()
}

fn prose(text: &str) -> Value {
    json!({"type":"response_item","payload":{"type":"message","role":"assistant",
        "content":[{"type":"output_text","text":text}]}})
}

fn normalized_text(text: &str) -> Value {
    json!({"type":"assistant","message":{"content":[{"type":"text","text":text}]}})
}

struct Fixture {
    channel: ChannelId,
    shared: Arc<SharedData>,
    health: Arc<HealthRegistry>,
    row: InflightTurnState,
    token: Arc<CancelToken>,
    handle: TmuxWatcherHandle,
    raw: PathBuf,
    wrapper: PathBuf,
    prefix: Vec<u8>,
    prefix_identity: (u64, u64),
    body_prefix: String,
    final_body: String,
    binding_before: Option<TuiRuntimeBinding>,
    marker_path: PathBuf,
    marker_before: Vec<u8>,
    gateway: MockDiscord,
    http_fail: Arc<AtomicBool>,
    failed_requests: Arc<AtomicU64>,
    watcher: Option<tokio::task::JoinHandle<()>>,
    supervisor: Option<tokio::task::JoinHandle<()>>,
    supervisor_shutdown: Arc<AtomicBool>,
    session_bound: bool,
    _rest: discord::shared_state::test_rest::Guard,
    _env: crate::config::TestEnvVarGuard,
    _root: tempfile::TempDir,
}

impl Fixture {
    async fn new(channel: u64, session_bound: bool) -> Self {
        let root = tempfile::tempdir().unwrap();
        let env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            root.path(),
        );
        let channel = ChannelId::new(channel);
        let tmux =
            ProviderKind::Codex.build_tmux_session_name(&format!("resume-entry-{}", channel.get()));
        let raw = root.path().join("rollout-entry.jsonl");
        let wrapper = PathBuf::from(crate::services::tmux_common::session_temp_path(
            &tmux, "jsonl",
        ));
        let generation = crate::services::tmux_common::session_temp_path(&tmux, "generation");
        std::fs::write(generation, b"entry-fixture-generation").unwrap();
        // Preserve the incident byte counts while omitting prompt/tool contents.
        let body_prefix = format!("SAVED_FROZEN_PREFIX|{}\n", "가".repeat(11_543));
        let final_body = format!("TERMINAL|{}", "f".repeat(690));
        assert_eq!(body_prefix.len(), 34_650);
        assert_eq!(final_body.len(), 699);
        let timestamp = (chrono::Utc::now() - chrono::Duration::seconds(10)).to_rfc3339();
        let mut native = vec![
            json!({"type":"session_meta","payload":{"id":SESSION,"cwd":root.path()}}),
            json!({"type":"event_msg","timestamp":timestamp,"payload":{"type":"task_started","turn_id":NATIVE_TURN}}),
            json!({"type":"turn_context","payload":{"turn_id":NATIVE_TURN}}),
            json!({"type":"response_item","timestamp":timestamp,"payload":{"type":"message","role":"user",
                "content":[{"type":"input_text","text":format!("[User: fixture (ID: {OWNER})] {PROMPT}")}]}}),
            prose(&body_prefix),
        ];
        native.extend((0..6).map(|_| json!({"type":"compacted","payload":{}})));
        std::fs::write(&raw, records(native)).unwrap();
        let prefix = records([
            json!({"type":"system","subtype":"init","session_id":SESSION}),
            normalized_text(&body_prefix),
        ]);
        std::fs::write(&wrapper, &prefix).unwrap();
        let metadata = std::fs::metadata(&wrapper).unwrap();
        let prefix_identity = (metadata.dev(), metadata.ino());
        let raw_eof = std::fs::metadata(&raw).unwrap().len();
        dedupe::register_tmux_runtime_binding(
            &tmux,
            TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::CodexTui,
                output_path: raw.to_string_lossy().into_owned(),
                relay_output_path: Some(wrapper.to_string_lossy().into_owned()),
                input_fifo_path: None,
                session_id: Some(SESSION.into()),
                last_offset: raw_eof,
                relay_last_offset: Some(prefix.len() as u64),
            },
        );
        crate::services::codex_tui::session::write_codex_tui_rollout_marker_with_start_offset(
            &tmux,
            &raw,
            Some(SESSION),
            Some(raw_eof),
        )
        .unwrap();
        let binding_before = dedupe::runtime_binding_for_tmux_session(&tmux);
        let marker_path = PathBuf::from(crate::services::tmux_common::session_temp_path(
            &tmux,
            crate::services::tmux_common::CODEX_TUI_ROLLOUT_MARKER_TEMP_EXT,
        ));
        let marker_before = std::fs::read(&marker_path).unwrap();
        let mut shared = discord::make_shared_data_for_tests();
        Arc::get_mut(&mut shared).unwrap().provider = ProviderKind::Codex;
        shared.settings.write().await.provider = ProviderKind::Codex;
        let health = Arc::new(HealthRegistry::new());
        health.register("codex".into(), shared.clone()).await;
        let token = Arc::new(CancelToken::new());
        token.bind_unmanaged_session_name(&tmux);
        assert!(
            Box::pin(discord::mailbox_try_start_turn(
                &shared,
                channel,
                token.clone(),
                UserId::new(OWNER),
                MessageId::new(channel.get() + 1)
            ))
            .await
        );
        shared.restart.global_active.store(1, Ordering::Release);
        let mut row = InflightTurnState::new(
            ProviderKind::Codex,
            channel.get(),
            Some("adk-cdx".into()),
            OWNER,
            channel.get() + 1,
            channel.get() + 2,
            PROMPT.into(),
            Some(SESSION.into()),
            Some(tmux.clone()),
            Some(wrapper.to_string_lossy().into_owned()),
            None,
            0,
        );
        row.runtime_kind = Some(RuntimeHandoffKind::CodexTui);
        row.native_turn_id = None;
        row.turn_nonce = token.turn_nonce().map(str::to_owned);
        row.started_at = (chrono::Local::now() - chrono::Duration::minutes(1))
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        row.updated_at = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        row.full_response = body_prefix.clone();
        row.response_sent_offset = 33_809;
        row.streaming_rollover_frozen_msg_ids = (10..18).map(|i| channel.get() + i).collect();
        row.set_relay_owner_kind(RelayOwnerKind::Watcher);
        inflight::save_inflight_state(&row).unwrap();
        assert!(
            Box::pin(discord::recovery_engine::reregister_active_turn_from_inflight(&shared, &row))
                .await,
            "restart fixture must reseed the real finalizer ledger for its original actor"
        );
        let row =
            inflight::load_inflight_state_read_only(&ProviderKind::Codex, channel.get()).unwrap();
        let handle = TmuxWatcherHandle {
            tmux_session_name: tmux.clone(),
            output_path: wrapper.to_string_lossy().into_owned(),
            paused: Arc::new(AtomicBool::new(false)),
            resume_offset: Arc::new(std::sync::Mutex::new(None)),
            cancel: Arc::new(AtomicBool::new(false)),
            pause_epoch: Arc::new(AtomicU64::new(0)),
            turn_delivered: Arc::new(AtomicBool::new(false)),
            last_heartbeat_ts_ms: Arc::new(AtomicI64::new(discord::tmux_watcher_now_ms())),
        };
        shared.tmux_watchers.insert(
            channel,
            TmuxWatcherHandle {
                tmux_session_name: handle.tmux_session_name.clone(),
                output_path: handle.output_path.clone(),
                paused: handle.paused.clone(),
                resume_offset: handle.resume_offset.clone(),
                cancel: handle.cancel.clone(),
                pause_epoch: handle.pause_epoch.clone(),
                turn_delivered: handle.turn_delivered.clone(),
                last_heartbeat_ts_ms: handle.last_heartbeat_ts_ms.clone(),
            },
        );
        let http_fail = Arc::new(AtomicBool::new(false));
        let failed_requests = Arc::new(AtomicU64::new(0));
        let (fail, failures) = (http_fail.clone(), failed_requests.clone());
        let gateway = MockDiscord::start_with(Arc::new(move |method, path| {
            if fail.load(Ordering::Acquire) && method != axum::http::Method::DELETE {
                failures.fetch_add(1, Ordering::AcqRel);
                return Some((500, json!({"message":"fixture unavailable","code":0})));
            }
            if method == axum::http::Method::DELETE {
                return None;
            }
            if method == axum::http::Method::GET
                && path.ends_with(&format!("/channels/{}", channel.get()))
            {
                return Some((
                    200,
                    json!({"id":channel.get().to_string(),"type":0,
                    "name":"resume-entry-cdx","guild_id":"7139000","position":0,
                    "permission_overwrites":[],"nsfw":false}),
                ));
            }
            let id = path
                .rsplit('/')
                .next()
                .and_then(|id| id.parse().ok())
                .unwrap_or(channel.get() + 100);
            Some((
                200,
                message_json(id, channel.get(), 1, "", "2026-10-19T00:00:00+00:00"),
            ))
        }))
        .await;
        let rest = discord::shared_state::test_rest::install(gateway.http.clone());
        let supervisor_shutdown = Arc::new(AtomicBool::new(false));
        let supervisor = session_bound.then(|| {
            let matched = MatchedChannel {
                channel_id: channel.get().to_string(),
                agent_id: "entry-fixture".into(),
                provider: ProviderKind::Codex,
                expected_session_name: tmux.clone(),
                expected_rollout_path: wrapper.to_string_lossy().into_owned(),
            };
            crate::services::cluster::session_registry::global_session_registry()
                .upsert(matched, None);
            tokio::spawn(discord::run_session_bound_discord_relay_supervisor(
                Some(health.clone()),
                supervisor_shutdown.clone(),
            ))
        });
        Self {
            channel,
            shared,
            health,
            row,
            token,
            handle,
            raw,
            wrapper,
            prefix,
            prefix_identity,
            body_prefix,
            final_body,
            binding_before,
            marker_path,
            marker_before,
            gateway,
            http_fail,
            failed_requests,
            watcher: None,
            supervisor,
            supervisor_shutdown,
            session_bound,
            _rest: rest,
            _env: env,
            _root: root,
        }
    }

    fn append_final(&self) {
        self.append_suffix();
        self.append_completion();
    }

    fn append_suffix(&self) {
        self.append_native(records([prose(&self.final_body)]));
    }

    fn append_completion(&self) {
        self.append_native(records([json!({"type":"event_msg","payload":{
            "type":"task_complete","turn_id":NATIVE_TURN,"last_agent_message":self.final_body}})]));
    }

    fn append_native(&self, bytes: Vec<u8>) {
        std::fs::OpenOptions::new()
            .append(true)
            .open(&self.raw)
            .unwrap()
            .write_all(&bytes)
            .unwrap();
    }

    fn pin_eof_hint(&mut self) {
        let eof = std::fs::metadata(&self.raw).unwrap().len();
        let mut binding =
            dedupe::runtime_binding_for_tmux_session(&self.handle.tmux_session_name).unwrap();
        binding.last_offset = eof;
        dedupe::register_tmux_runtime_binding(&self.handle.tmux_session_name, binding);
        crate::services::codex_tui::session::write_codex_tui_rollout_marker_with_start_offset(
            &self.handle.tmux_session_name,
            &self.raw,
            Some(SESSION),
            Some(eof),
        )
        .unwrap();
        self.binding_before =
            dedupe::runtime_binding_for_tmux_session(&self.handle.tmux_session_name);
        self.marker_before = std::fs::read(&self.marker_path).unwrap();
    }

    fn start_watcher(&mut self, supervised: bool) {
        let h = &self.handle;
        let restored = discord::tmux::restored_watcher_turn_from_inflight(
            &self.row,
            &h.tmux_session_name,
            true,
        );
        let future = Box::pin(discord::tmux::tmux_output_watcher_with_restore(
            self.channel,
            self.gateway.http.clone(),
            self.shared.clone(),
            h.output_path.clone(),
            h.tmux_session_name.clone(),
            0,
            h.cancel.clone(),
            h.paused.clone(),
            h.resume_offset.clone(),
            h.pause_epoch.clone(),
            h.turn_delivered.clone(),
            h.last_heartbeat_ts_ms.clone(),
            restored,
        ));
        self.watcher = Some(if supervised {
            discord::task_supervisor::spawn_observed_tmux_watcher(
                "producer-entry-test",
                self.shared.clone(),
                h.tmux_session_name.clone(),
                h.cancel.clone(),
                future,
            )
        } else {
            tokio::spawn(future)
        });
    }

    async fn periodic(&self) {
        discord::health::run_orphan_token_auto_heal_pass_for_tests(
            &self.health,
            &ProviderKind::Codex,
            std::slice::from_ref(&self.shared),
        )
        .await;
    }

    async fn wait_result(&self) -> Value {
        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                let bytes = std::fs::read(&self.wrapper).unwrap();
                if let Some(result) = bytes
                    .split(|b| *b == b'\n')
                    .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
                    .find(|v| v["type"] == "result")
                {
                    return result;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("production entry must revive producer and append exact native terminal")
    }

    async fn wait_suffix(&self) {
        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                let bytes = std::fs::read(&self.wrapper).unwrap();
                let rows = bytes
                    .split(|b| *b == b'\n')
                    .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
                    .collect::<Vec<_>>();
                assert!(
                    rows.iter().all(|v| v["type"] != "result"),
                    "an incomplete native turn must not synthesize completion"
                );
                if rows.iter().any(|v| {
                    v.pointer("/message/content/0/text").and_then(Value::as_str)
                        == Some(self.final_body.as_str())
                }) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("periodic entry must restore streaming before native task_complete");
    }

    fn assert_source_preserved(&self) {
        let bytes = std::fs::read(&self.wrapper).unwrap();
        assert!(
            bytes.starts_with(&self.prefix),
            "old wrapper bytes must remain identical"
        );
        let metadata = std::fs::metadata(&self.wrapper).unwrap();
        assert_eq!((metadata.dev(), metadata.ino()), self.prefix_identity);
        assert_eq!(
            dedupe::runtime_binding_for_tmux_session(&self.handle.tmux_session_name),
            self.binding_before,
            "producer revival must not reinstall or advance runtime binding"
        );
        assert_eq!(
            std::fs::read(&self.marker_path).unwrap(),
            self.marker_before,
            "raw EOF marker is neither an append receipt nor a recovery starting point"
        );
        assert_eq!(
            bytes
                .split(|b| *b == b'\n')
                .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
                .filter(|v| v["type"] == "result")
                .count(),
            1
        );
    }

    async fn assert_settled(&self) {
        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                if inflight::load_inflight_state_read_only(&ProviderKind::Codex, self.channel.get())
                    .is_none()
                    && discord::mailbox_snapshot(&self.shared, self.channel)
                        .await
                        .cancel_token
                        .is_none()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("existing watcher must deliver, durably commit and settle original actor");
        let receipt = delivery_record::read_record(&ProviderKind::Codex, self.channel.get())
            .expect("durable receipt");
        assert!(
            receipt
                .confirmed_deliveries
                .iter()
                .any(
                    |r| r.source.turn_nonce == self.row.turn_nonce.as_deref().unwrap()
                        && r.source.range.1 <= std::fs::metadata(&self.wrapper).unwrap().len()
                ),
            "same actor receipt must use wrapper byte coordinates"
        );
        let ledger = completed_turn_ledger::read_ledger(&ProviderKind::Codex, self.channel.get())
            .expect("completion ledger");
        assert_eq!(
            ledger
                .entries
                .iter()
                .filter(|e| e.user_msg_id == self.row.user_msg_id)
                .count(),
            1
        );
        assert_eq!(self.shared.restart.global_active.load(Ordering::Acquire), 0);
        assert!(
            !self.token.cancelled.load(Ordering::Acquire),
            "successful revival must not cancel original turn"
        );
        let requests = self.gateway.requests_for(self.channel.get());
        let mutations: Vec<_> = requests
            .iter()
            .filter(|r| r.starts_with("POST ") || r.starts_with("PATCH "))
            .collect();
        assert!(
            mutations
                .iter()
                .all(|r| !r.contains("SAVED_FROZEN_PREFIX|")),
            "a saved frozen prefix must never be reposted: {mutations:?}"
        );
        assert_eq!(
            mutations
                .iter()
                .filter(|r| r.contains(&self.final_body))
                .count(),
            1,
            "the 699-byte native final must appear in exactly one Discord mutation: {mutations:?}"
        );
    }

    fn mutations(&self) -> Vec<String> {
        self.gateway
            .requests_for(self.channel.get())
            .into_iter()
            .filter(|request| request.starts_with("POST ") || request.starts_with("PATCH "))
            .collect()
    }

    async fn assert_idle_does_not_replay(&mut self) {
        assert!(self.session_bound);
        let before = self.mutations();
        // The actual idle loop uses std::time::Instant for its ten-second grace.
        tokio::time::sleep(Duration::from_secs(11)).await;
        assert_eq!(
            self.mutations(),
            before,
            "idle catch-up must not repost terminal suffix after grace"
        );
        self.supervisor_shutdown.store(true, Ordering::Release);
        self.supervisor.take().unwrap().await.unwrap();
        self.supervisor_shutdown = Arc::new(AtomicBool::new(false));
        self.supervisor = Some(tokio::spawn(
            discord::run_session_bound_discord_relay_supervisor(
                Some(self.health.clone()),
                self.supervisor_shutdown.clone(),
            ),
        ));
        tokio::time::sleep(Duration::from_secs(11)).await;
        assert_eq!(
            self.mutations(),
            before,
            "fresh idle reader must consult committed wrapper frontier after restart"
        );
    }

    async fn stop(mut self) {
        self.handle.cancel.store(true, Ordering::Release);
        if let Some(task) = self.watcher.take() {
            task.abort();
            let _ = task.await;
        }
        self.supervisor_shutdown.store(true, Ordering::Release);
        if let Some(task) = self.supervisor.take() {
            task.await.unwrap();
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.handle.cancel.store(true, Ordering::Release);
        if let Some(task) = &self.watcher {
            task.abort();
        }
        self.supervisor_shutdown.store(true, Ordering::Release);
        if let Some(task) = &self.supervisor {
            task.abort();
        }
        crate::services::cluster::session_registry::global_session_registry()
            .remove(&self.handle.tmux_session_name);
        crate::services::cluster::relay_producer_registry::global_relay_producer_registry()
            .deregister(&self.handle.tmux_session_name);
        dedupe::clear_tmux_runtime_binding(&self.handle.tmux_session_name);
    }
}

async fn restart_entry(channel: u64, session_bound: bool) {
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let mut f = Box::pin(Fixture::new(channel, session_bound)).await;
    f.append_final();
    f.pin_eof_hint();
    f.start_watcher(true);
    let result = f.wait_result().await;
    assert_eq!(
        result["result"],
        format!("{}{}", f.body_prefix, f.final_body),
        "strict-skipped JSON text must remain in the terminal accumulated response"
    );
    f.assert_source_preserved();
    f.assert_settled().await;
    if session_bound {
        f.assert_idle_does_not_replay().await;
    }
    let before = f.mutations();
    f.periodic().await;
    f.periodic().await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(f.mutations(), before, "settled reticks must not redeliver");
    f.assert_source_preserved();
    f.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn r1_supervised_restart_restores_direct_watcher_producer_and_final() {
    Box::pin(restart_entry(7_139_101, false)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn r1_supervised_restart_restores_session_bound_producer_and_final() {
    Box::pin(restart_entry(7_139_102, true)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn r1_supervised_restart_preserves_skipped_text_before_terminal_only_fallback() {
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let mut f = Box::pin(Fixture::new(7_139_104, false)).await;
    // No assistant suffix record: only task_complete carries the missing native final.
    f.append_completion();
    f.pin_eof_hint();
    f.start_watcher(true);
    let result = f.wait_result().await;
    assert_eq!(
        result["result"],
        format!("{}{}", f.body_prefix, f.final_body)
    );
    let bytes = std::fs::read(&f.wrapper).unwrap();
    let appended = &bytes[f.prefix.len()..];
    let rows: Vec<Value> = appended
        .split(|b| *b == b'\n')
        .filter_map(|line| serde_json::from_slice(line).ok())
        .collect();
    assert_eq!(
        rows.len(),
        2,
        "fallback must append its missing text before one result"
    );
    assert_eq!(
        rows[0],
        normalized_text(&f.final_body),
        "skipped JSON text must accumulate so Done yields exactly the missing suffix"
    );
    assert_eq!(rows[1], result);
    f.assert_source_preserved();
    f.assert_settled().await;
    f.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn r2_periodic_revives_live_incumbent_despite_fresh_row_and_open_circuit() {
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let mut f = Box::pin(Fixture::new(7_139_103, false)).await;
    let circuit = discord::runtime_store::runtime_root()
        .unwrap()
        .join("discord_relay_recovery_circuit/codex")
        .join(format!("{}.json", f.channel.get()));
    std::fs::create_dir_all(circuit.parent().unwrap()).unwrap();
    let state = json!({"version":1,"episode_key":discord::relay_recovery::circuit_evidence::episode_key(&f.row),
        "baseline_relay_offset":0,"attempts":2,"alert_queued":true,"staged_alert_id":null,
        "orphaned_staged_alert_ids":[],"open_generation":7,"coordinate_binding":"Unresolved"});
    std::fs::write(&circuit, serde_json::to_vec(&state).unwrap()).unwrap();
    // This task predates revival; calling the startup wrapper would hide a missing periodic hook.
    f.start_watcher(false);
    let owner_cancel = f.handle.cancel.clone();
    f.append_suffix();
    f.periodic().await;
    f.periodic().await;
    f.wait_suffix().await;
    assert_eq!(
        serde_json::from_slice::<Value>(&std::fs::read(&circuit).unwrap()).unwrap(),
        state,
        "producer repair must not spend/reset durable reattach budget"
    );
    assert!(
        f.shared
            .tmux_watchers
            .get(&f.channel)
            .is_some_and(|h| Arc::ptr_eq(&h.cancel, &owner_cancel)),
        "periodic repair must retain the incumbent watcher allocation"
    );
    f.append_completion();
    let result = f.wait_result().await;
    assert_eq!(
        result["result"],
        format!("{}{}", f.body_prefix, f.final_body)
    );
    f.assert_source_preserved();
    f.assert_settled().await;
    f.periodic().await;
    f.assert_source_preserved();
    f.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn r6_contention_retains_event_order_and_clear_blocks_old_output() {
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let f = Box::pin(Fixture::new(7_139_106, false)).await;
    assert_eq!(
        super::ensure(
            f.shared.clone(),
            f.handle.tmux_session_name.clone(),
            f.handle.cancel.clone()
        )
        .await,
        super::ResumeOutcome::WriterReady
    );
    let row_path = inflight::inflight_state_path(
        &inflight::inflight_runtime_root().unwrap(),
        &ProviderKind::Codex,
        f.channel.get(),
    );
    let row_lock = inflight::try_lock_inflight_state_path(&row_path).unwrap();
    f.append_native(records([prose("first"), prose("second")]));
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        std::fs::read(&f.wrapper).unwrap(),
        f.prefix,
        "contended event stays pending"
    );
    drop(row_lock);
    tokio::time::timeout(Duration::from_secs(3), async {
        while std::fs::read(&f.wrapper)
            .unwrap()
            .split(|b| *b == b'\n')
            .filter(|line| !line.is_empty())
            .count()
            < 4
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let delivered = std::fs::read(&f.wrapper).unwrap();
    let text: Vec<_> = delivered
        .split(|b| *b == b'\n')
        .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
        .collect();
    assert_eq!(text[2].pointer("/message/content/0/text").unwrap(), "first");
    assert_eq!(
        text[3].pointer("/message/content/0/text").unwrap(),
        "\n\nsecond"
    );
    // A cleared episode has no publication authority even if its native producer keeps growing.
    std::fs::remove_file(row_path).unwrap();
    f.append_final();
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(
        std::fs::read(&f.wrapper).unwrap(),
        delivered,
        "old output cannot reappear after clear"
    );
    assert!(f.gateway.requests_for(f.channel.get()).is_empty());
    f.stop().await;
}

#[test]
fn r9_applicability_requires_old_codex_wrapper_episode_and_retains_scope_fences() {
    let tmux = "resume-scope";
    let wrapper = crate::services::tmux_common::session_temp_path(tmux, "jsonl");
    let mut row = InflightTurnState::new(
        ProviderKind::Codex,
        7139107,
        None,
        OWNER,
        11,
        12,
        PROMPT.into(),
        Some(SESSION.into()),
        Some(tmux.into()),
        Some(wrapper),
        None,
        0,
    );
    row.runtime_kind = Some(RuntimeHandoffKind::CodexTui);
    row.set_relay_owner_kind(RelayOwnerKind::Watcher);
    row.turn_start_offset = Some(0);
    assert!(super::applicable(&row, tmux, 10));
    assert!(!super::applicable(&row, tmux, 0));
    let original = row.clone();
    row.runtime_kind = Some(RuntimeHandoffKind::LegacyTmuxWrapper);
    assert!(!super::applicable(&row, tmux, 10));
    row = original.clone();
    row.output_path = Some("/native/rollout.jsonl".into());
    assert!(!super::applicable(&row, tmux, 10));
    row = original.clone();
    row.runtime_kind_unknown_on_disk = true;
    assert!(!super::applicable(&row, tmux, 10));
    row = original.clone();
    row.version = u32::MAX;
    assert!(!super::applicable(&row, tmux, 10));
    row = original.clone();
    row.terminal_delivery_committed = true;
    assert!(!super::applicable(&row, tmux, 10));
    row = original;
    row.turn_source = inflight::TurnSource::ExternalInput;
    assert!(!super::applicable(&row, tmux, 10));
}

thread_local! {
    static ENSURE_HOOK: std::cell::Cell<u8> = const { std::cell::Cell::new(0) };
}
pub(super) async fn before_ensure() {
    match ENSURE_HOOK.with(|hook| hook.replace(0)) {
        1 => panic!("injected producer startup panic"),
        2 => tokio::time::sleep(Duration::from_secs(5)).await,
        _ => {}
    }
}

#[tokio::test(flavor = "current_thread")]
async fn r12_ensure_panic_uses_watcher_cleanup_and_completion() {
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let f = Box::pin(Fixture::new(7_139_108, false)).await;
    ENSURE_HOOK.with(|hook| hook.set(1));
    let task = discord::task_supervisor::spawn_observed_tmux_watcher(
        "producer-panic",
        f.shared.clone(),
        f.handle.tmux_session_name.clone(),
        f.handle.cancel.clone(),
        async {
            panic!("watcher must not poll after ensure panic");
        },
    );
    let ticket = discord::task_supervisor::watcher_completion::observe(&f.handle.cancel).unwrap();
    assert_eq!(
        ticket.wait().await,
        discord::task_supervisor::watcher_completion::Outcome::Panicked
    );
    task.await.unwrap();
    assert!(f.shared.tmux_watchers.get(&f.channel).is_none());
    assert_eq!(std::fs::read(&f.wrapper).unwrap(), f.prefix);
    assert!(!f.token.cancelled.load(Ordering::Acquire));
    f.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn r12_slow_ensure_does_not_delay_watcher_first_poll_past_startup_budget() {
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let f = Box::pin(Fixture::new(7_139_109, false)).await;
    let (polled, first_poll) = tokio::sync::oneshot::channel();
    ENSURE_HOOK.with(|hook| hook.set(2));
    let task = discord::task_supervisor::spawn_observed_tmux_watcher(
        "producer-budget",
        f.shared.clone(),
        f.handle.tmux_session_name.clone(),
        f.handle.cancel.clone(),
        async move {
            let _ = polled.send(());
        },
    );
    tokio::time::timeout(Duration::from_millis(750), first_poll)
        .await
        .unwrap()
        .unwrap();
    task.await.unwrap();
    assert_eq!(std::fs::read(&f.wrapper).unwrap(), f.prefix);
    f.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn r8_http_failure_keeps_original_actor_until_receipt_commit() {
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let mut f = Box::pin(Fixture::new(7_139_110, false)).await;
    f.http_fail.store(true, Ordering::Release);
    f.append_final();
    f.start_watcher(true);
    f.wait_result().await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while f.failed_requests.load(Ordering::Acquire) == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        inflight::load_inflight_state_read_only(&ProviderKind::Codex, f.channel.get()).is_some()
    );
    assert!(
        discord::mailbox_snapshot(&f.shared, f.channel)
            .await
            .cancel_token
            .is_some()
    );
    assert_eq!(f.shared.restart.global_active.load(Ordering::Acquire), 1);
    assert!(
        delivery_record::read_record(&ProviderKind::Codex, f.channel.get())
            .is_none_or(|r| r.confirmed_deliveries.is_empty())
    );
    f.http_fail.store(false, Ordering::Release);
    f.assert_settled().await;
    f.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn r6_live_publication_stops_on_episode_cancel_reset_or_source_guard_change() {
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    for (index, change) in [
        "successor",
        "current_msg",
        "owner",
        "cancel",
        "epoch",
        "reset",
        "binding",
        "marker",
    ]
    .into_iter()
    .enumerate()
    {
        let f = Box::pin(Fixture::new(7_139_120 + index as u64, false)).await;
        let coord = f.shared.tmux_relay_coord(f.channel);
        coord.advance_untyped_frontier(100, None);
        assert_eq!(
            super::ensure(
                f.shared.clone(),
                f.handle.tmux_session_name.clone(),
                f.handle.cancel.clone()
            )
            .await,
            super::ResumeOutcome::WriterReady,
            "{change}"
        );
        match change {
            "successor" | "current_msg" | "owner" => {
                let root = inflight::inflight_runtime_root().unwrap();
                let path =
                    inflight::inflight_state_path(&root, &ProviderKind::Codex, f.channel.get());
                let _lock = inflight::try_lock_inflight_state_path(&path).unwrap();
                let mut row =
                    inflight::load_inflight_state_read_only(&ProviderKind::Codex, f.channel.get())
                        .unwrap();
                match change {
                    "successor" => row.user_msg_id += 1000,
                    "current_msg" => row.current_msg_id += 1000,
                    _ => row.request_owner_user_id += 1000,
                }
                std::fs::write(path, serde_json::to_vec(&row).unwrap()).unwrap();
            }
            "cancel" => f.handle.cancel.store(true, Ordering::Release),
            "epoch" => {
                f.handle.pause_epoch.fetch_add(1, Ordering::AcqRel);
            }
            "reset" => {
                assert!(coord.reset_confirmed_frontier(100, 0));
            }
            "binding" => {
                dedupe::clear_tmux_runtime_binding(&f.handle.tmux_session_name);
            }
            "marker" => std::fs::remove_file(&f.marker_path).unwrap(),
            _ => unreachable!(),
        }
        f.append_final();
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(
            std::fs::read(&f.wrapper).unwrap(),
            f.prefix,
            "old event escaped {change}"
        );
        assert!(f.gateway.requests_for(f.channel.get()).is_empty());
        f.stop().await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn r11_incident_shaped_pending_row_still_rejects_whole_rebind_without_mutation() {
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let f = Box::pin(Fixture::new(7_139_130, false)).await;
    let root = inflight::inflight_runtime_root().unwrap();
    let row_path = inflight::inflight_state_path(&root, &ProviderKind::Codex, f.channel.get());
    let before = std::fs::read(&row_path).unwrap();
    let result = discord::recovery_engine::rebind_inflight_for_channel(
        &f.gateway.http,
        &f.shared,
        &ProviderKind::Codex,
        f.channel.get(),
        Some(f.handle.tmux_session_name.clone()),
        discord::recovery_engine::ManualRebindOverrides::default(),
        None,
    )
    .await;
    assert!(matches!(
        result,
        Err(discord::recovery_engine::RebindError::InflightAlreadyExists)
    ));
    assert_eq!(std::fs::read(row_path).unwrap(), before);
    assert_eq!(std::fs::read(&f.wrapper).unwrap(), f.prefix);
    assert_eq!(std::fs::read(&f.marker_path).unwrap(), f.marker_before);
    assert_eq!(
        dedupe::runtime_binding_for_tmux_session(&f.handle.tmux_session_name),
        f.binding_before
    );
    assert!(!super::generations::present(f.wrapper.to_str().unwrap()));
    assert!(
        discord::mailbox_snapshot(&f.shared, f.channel)
            .await
            .cancel_token
            .is_some()
    );
    assert!(f.gateway.requests_for(f.channel.get()).is_empty());
    f.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn r1_repaired_episode_admits_one_queued_successor_and_delivers_its_final() {
    use crate::services::discord::router::message_handler as intake;
    use crate::services::turn_orchestrator::{Intervention, InterventionMode};
    use std::time::Instant;

    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let mut f = Box::pin(Fixture::new(7_139_140, false)).await;
    f.append_final();
    f.pin_eof_hint();
    f.start_watcher(true);
    f.wait_result().await;
    f.assert_source_preserved();
    f.assert_settled().await;

    let _ready = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
        "ADK_TEST_CODEX_PROMOTE_READY",
        "1".as_ref(),
    );
    let _routing = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
        "ADK_INTAKE_ROUTING_MODE",
        "disabled".as_ref(),
    );
    let config = f._root.path().join("successor-config.yml");
    std::fs::write(&config, "server: {}\ndata: {dir: data}\n").unwrap();
    let _config = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_CONFIG",
        &config,
    );
    discord::host_defer_gate::tests::map_channel(&f.shared, f.channel, "").await;
    {
        let mut core = f.shared.core.lock().await;
        let session = core.sessions.get_mut(&f.channel).unwrap();
        session.channel_name = None;
        session.current_path = Some(f._root.path().display().to_string());
        session.session_id = None;
    }
    f.shared.settings.write().await.provider = ProviderKind::Codex;
    let next_id = MessageId::new(f.channel.get() + 500);
    let next = Intervention {
        author_id: UserId::new(1),
        author_is_bot: false,
        message_id: next_id,
        queued_generation: discord::runtime_store::process_generation(),
        source_message_ids: vec![next_id],
        source_message_queued_generations: Vec::new(),
        source_text_segments: Vec::new(),
        text: "queued successor after repaired episode".into(),
        mode: InterventionMode::Soft,
        created_at: Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
        voice_announcement: None,
    };
    let queued = discord::queue_io::with_post_enqueue_idle_queue_kick_suppressed(
        discord::mailbox_enqueue_intervention(&f.shared, &ProviderKind::Codex, f.channel, next),
    )
    .await;
    assert!(queued.enqueued && queued.persistence_error.is_none());
    let before = f.mutations();
    let final_body = "QUEUED_SUCCESSOR_FINAL_7139";
    let _result = intake::input_result_for_tests((f.channel.get(), final_body.into()));
    let (entered, release) = intake::hold_input_effect_for_tests(f.channel.get());
    let deps = intake::IntakeDeps {
        http: &f.gateway.http,
        cache: None,
        ctx_for_chained_dispatch: None,
        shared: &f.shared,
        token: "test-token",
    };
    let run = async {
        tokio::time::timeout(Duration::from_secs(15), entered)
            .await
            .unwrap()
            .unwrap();
        let snapshot = discord::mailbox_snapshot(&f.shared, f.channel).await;
        assert_eq!(snapshot.active_user_message_id, Some(next_id));
        assert!(snapshot.intervention_queue.is_empty());
        let row =
            inflight::load_inflight_state_read_only(&ProviderKind::Codex, f.channel.get()).unwrap();
        assert_eq!(row.user_msg_id, next_id.get());
        assert_ne!(row.user_msg_id, f.row.user_msg_id);
        release.send(()).unwrap();
    };
    let kickoff = Box::pin(discord::kickoff_idle_queue_channel(
        &deps,
        &ProviderKind::Codex,
        f.channel,
    ));
    let (outcome, ()) = tokio::join!(kickoff, run);
    assert!(
        outcome.started,
        "the actual queued admission reached the provider driver"
    );
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let snapshot = discord::mailbox_snapshot(&f.shared, f.channel).await;
            let sent = f
                .mutations()
                .iter()
                .filter(|request| request.contains(final_body))
                .count();
            if snapshot.cancel_token.is_none()
                && sent == 1
                && inflight::load_inflight_state_read_only(&ProviderKind::Codex, f.channel.get())
                    .is_none()
            {
                assert!(snapshot.intervention_queue.is_empty());
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the queued successor must deliver its final and settle its own actor");
    assert_eq!(
        f.mutations()
            .iter()
            .filter(|r| r.contains(final_body))
            .count(),
        1
    );
    assert!(f.mutations().starts_with(&before));
    let after = f.mutations();
    assert!(
        !discord::kickoff_idle_queue_channel(&deps, &ProviderKind::Codex, f.channel)
            .await
            .started
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        f.mutations(),
        after,
        "the successor cannot be admitted or delivered twice"
    );
    f.stop().await;
}
