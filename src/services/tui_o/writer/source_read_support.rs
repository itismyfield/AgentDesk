//! Isolated fixtures for verified source reads.

use super::{actor, binding::ChannelBindingLog};
use crate::config::TestEnvVarGuard;
use crate::services::{
    claude_tui::hook_server::observation_ingress::observe_binding_hook,
    tmux_common as tc,
    tui_o::{
        shadow::ShadowProvider,
        store::{ChannelStore, InitSource, Initialized, OStore, StoreConfig, ledger::PieceOutcome},
    },
    tui_prompt_dedupe::{
        self as dedupe,
        binding_context::{
            BINDING_HEADER, BindingContext, CapturedContext, HookBindingEnvelope,
            ObservedHookProcess, PreparedIncarnation,
        },
        binding_events,
    },
};
use chrono::{DateTime, Duration, Utc};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

pub(super) const A: &str = "019e660d-4859-7522-9cee-8ba7c4e7c743";
pub const PROMPT: &str = "read verified source";

pub struct Fixture {
    pub(super) fake: crate::services::provider_teardown::test_support::FakeTmux,
    pub context: BindingContext,
    pub root: tempfile::TempDir,
    _env: [TestEnvVarGuard; 2],
    _dedupe: std::sync::MutexGuard<'static, ()>,
    _shared: crate::config::test_env_lock::SharedTestEnvLockGuard,
}

impl Fixture {
    pub fn new(verified: bool) -> Self {
        let shared = crate::config::test_env_lock::acquire_shared_test_env_lock();
        Self::with_env_lock(verified, shared)
    }

    pub(super) fn with_env_lock(
        verified: bool,
        shared: crate::config::test_env_lock::SharedTestEnvLockGuard,
    ) -> Self {
        let lock = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (root, env) =
            dedupe::binding_context::test_support::fixture_after_shared_test_env_lock();
        dedupe::reset_state_for_tests();
        binding_events::set_test_root(Some(root.path()));
        let sessions = root.path().join("sessions");
        fs::create_dir(&sessions).unwrap();
        let context = BindingContext {
            schema: 1,
            provider: "codex".into(),
            created_at: Utc::now(),
            execution_nonce: uuid::Uuid::new_v4().simple().to_string(),
            tmux_session: if verified {
                crate::services::codex_tui::canary::CANARY_TMUX.into()
            } else {
                format!("legacy-{}", uuid::Uuid::new_v4().simple())
            },
            channel_id: Some(if verified {
                crate::services::codex_tui::canary::CANARY_CHANNEL
            } else {
                584_599
            }),
            owner_runtime_root: tc::current_tmux_owner_marker(),
            host: None,
            expected_native_session_id: None,
            launch_mode: "fresh".into(),
            provider_root: Some(sessions.canonicalize().unwrap()),
            first_prompt_digest: Some(format!("sha256:{:x}", Sha256::digest(PROMPT.as_bytes()))),
            source_policy: Some(if verified { "verified" } else { "legacy" }.into()),
            resume_start: None,
        };
        PreparedIncarnation::create(context.clone()).unwrap();
        let marker = tc::session_temp_path(&context.tmux_session, "spawn_nonce");
        fs::create_dir_all(Path::new(&marker).parent().unwrap()).unwrap();
        fs::write(marker, &context.execution_nonce).unwrap();
        fs::write(
            tc::session_temp_path(&context.tmux_session, tc::TMUX_RUNTIME_KIND_TEMP_EXT),
            crate::services::agent_protocol::RuntimeHandoffKind::CodexTui.as_str(),
        )
        .unwrap();
        dedupe::register_tmux_channel(&context.tmux_session, context.channel_id.unwrap());
        let fake = crate::services::provider_teardown::test_support::FakeTmux::install(
            &context.tmux_session,
        );
        Self {
            fake,
            context,
            root,
            _env: env,
            _dedupe: lock,
            _shared: shared,
        }
    }
    pub fn path(&self, id: &str) -> PathBuf {
        self.context
            .provider_root
            .as_ref()
            .unwrap()
            .join(format!("rollout-{id}.jsonl"))
    }
    pub(super) fn header(&self, id: &str) {
        self.header_at(id, self.time(1));
    }
    pub fn header_at(&self, id: &str, time: DateTime<Utc>) {
        let time = time.to_rfc3339();
        let meta = json!({"type":"session_meta", "timestamp":time, "payload":{"id":id,"timestamp":time,"cwd":self.root.path(),"source":"cli","originator":"codex_cli_rs"}});
        fs::write(self.path(id), format!("{meta}\n")).unwrap();
    }
    pub(super) fn send(
        &self,
        id: &str,
        event: &str,
        reason: &str,
        time: Option<DateTime<Utc>>,
        context: Option<BindingContext>,
    ) {
        let envelope = HookBindingEnvelope {
            context: CapturedContext::Captured(
                context.unwrap_or_else(|| self.context.clone()).into(),
            ),
            observed: ObservedHookProcess::default(),
        };
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(BINDING_HEADER, envelope.encode().unwrap().parse().unwrap());
        if let Some(time) = time {
            headers.insert(
                "x-agentdesk-relay-published-at",
                time.to_rfc3339().parse().unwrap(),
            );
        }
        let payload = json!({"session_id":id,"transcript_path":self.path(id),"source":reason,"prompt":PROMPT});
        let _ = observe_binding_hook("codex", event, Some(id), Some(id), &payload, &headers);
    }
    pub(super) fn time(&self, n: i64) -> DateTime<Utc> {
        self.context.created_at + Duration::seconds(n)
    }
    pub fn log_bytes(&self) -> Vec<u8> {
        fs::read(
            self.root
                .path()
                .join(binding_events::BINDING_EVENTS_DIR)
                .join(format!("{}.log", self.context.channel_id.unwrap())),
        )
        .unwrap()
    }
    pub(super) fn next_fresh(&mut self, created_at: DateTime<Utc>) -> BindingContext {
        let previous = self.context.clone();
        let log = self.log_bytes();
        self.context.created_at = created_at;
        self.context.execution_nonce = uuid::Uuid::new_v4().simple().to_string();
        PreparedIncarnation::create(self.context.clone()).unwrap();
        fs::write(
            tc::session_temp_path(&self.context.tmux_session, "spawn_nonce"),
            &self.context.execution_nonce,
        )
        .unwrap();
        dedupe::reset_state_for_tests();
        dedupe::register_tmux_channel(&self.context.tmux_session, self.context.channel_id.unwrap());
        assert_eq!(
            self.log_bytes(),
            log,
            "new execution must retain durable history"
        );
        assert!(self.fold().verified.is_none());
        previous
    }
    pub fn fold(&self) -> binding_events::codex::Fold {
        binding_events::codex::read_ownership(&self.context).unwrap()
    }
    pub(super) fn current(&self) -> dedupe::TuiRuntimeBinding {
        dedupe::runtime_binding_for_tmux_session(&self.context.tmux_session)
            .expect("production Unknown must read its verified source")
    }
    pub(super) fn marker(&self) -> Vec<u8> {
        fs::read(tc::session_temp_path(
            &self.context.tmux_session,
            tc::CODEX_TUI_ROLLOUT_MARKER_TEMP_EXT,
        ))
        .unwrap()
    }
    pub fn log(&self) -> ChannelBindingLog {
        ChannelBindingLog::new(self.context.channel_id.unwrap(), ShadowProvider::Codex)
    }
    pub(super) fn store(&self) -> (OStore, ChannelStore) {
        let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, self.root.path())
            .unwrap()
            .unwrap();
        let source = self.fold().verified.unwrap().source;
        let channel = self.context.channel_id.unwrap();
        store
            .begin_fixture_era(&[channel], Utc::now(), |channel| {
                Ok(Initialized {
                    policy_parent: None,
                    channel,
                    sources: vec![InitSource {
                        source_id: source.clone(),
                        delivery_start: 0,
                        prefix_hash: hex::encode(Sha256::digest(b"")),
                    }],
                    initial_anchor: 100,
                    build_digest: "source-read".into(),
                    at: Utc::now(),
                })
            })
            .unwrap();
        let era = store.read_era().unwrap().unwrap();
        let opened = store.open_channel(&era, channel).unwrap().unwrap();
        (store, opened)
    }
    pub fn reopened(&self, store: &OStore) -> ChannelStore {
        store
            .open_channel(
                &store.read_era().unwrap().unwrap(),
                self.context.channel_id.unwrap(),
            )
            .unwrap()
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        binding_events::forget_channel_for_tests(self.context.channel_id.unwrap());
        binding_events::set_test_root(None);
        dedupe::reset_state_for_tests();
    }
}
pub(super) fn append(path: &Path, text: &str) {
    fs::OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(text.as_bytes())
        .unwrap();
}
pub(super) fn unit(id: &str, text: &str) -> String {
    format!(
        "{}\n",
        json!({"type":"response_item","payload":{"type":"message","role":"assistant","id":id,"content":[{"type":"output_text","text":text}]}})
    )
}
pub fn response(path: &Path, id: &str, text: &str) {
    append(path, &unit(id, text));
    append(
        path,
        &format!(
            "{}\n",
            json!({"type":"event_msg","payload":{"type":"task_complete","last_agent_message":text}})
        ),
    );
}
pub async fn polls() {
    tokio::time::sleep(actor::POLL_INTERVAL * 3).await;
}

pub fn posted(store: &ChannelStore) -> usize {
    let ledger = store.ledger();
    (0..ledger.next_serial())
        .filter_map(|serial| ledger.piece(serial))
        .filter(|piece| matches!(piece.outcome, Some(PieceOutcome::Posted(_))))
        .count()
}
