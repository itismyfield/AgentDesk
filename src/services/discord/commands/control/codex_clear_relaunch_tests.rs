//! A clear retires the canary for launching while its old source remains protected.
#![cfg(unix)]

use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::dispatched_session_canonical_identity::{
    CanonicalSessionIdentity, SessionIdentityKind, upsert_hook_session_with_identity_pg,
};
use crate::db::dispatched_sessions::HookSessionUpsert;
use crate::services::claude_tui::hook_server::adoption_retry::DurableKind;
use crate::services::claude_tui::hook_server::observation_ingress::{
    IngressOutcome, observe_binding_hook,
};
use crate::services::codex::{CodexSourceMode, SOURCE_MODE_TEST};
use crate::services::codex_tui::canary::{CANARY_CHANNEL, CANARY_TMUX};
use crate::services::discord::turn_presence::activity::{self, Activity};
use crate::services::provider_teardown::tests::test_support::FakeTmux;
use crate::services::tui_o::turn_mode::TestConfirmation;
use crate::services::tui_prompt_dedupe::binding_context::{
    BINDING_HEADER, BindingContext, CapturedContext, HookBindingEnvelope, ObservedHookProcess,
    PreparedIncarnation, SpawnNonceMarker,
};
use crate::services::tui_prompt_dedupe::{self as dedupe, binding_events as p5};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub(in crate::services::discord) const NATIVE_ID: &str = "019e660d-4859-7522-9cee-8ba7c4e7c743";
const PROMPT: &str = "canary clear before the first new input\n한글";
const AFTER_CLEAR: &str = "first instruction after clear";
pub(in crate::services::discord) const NEW_NATIVE_ID: &str = "019e660d-4859-7522-9cee-8ba7c4e7c744";
const TEST_BOT: &str = "e51-isolated-test-bot";

pub(in crate::services::discord) mod absent_launch_tests;

struct SourceMode(Option<CodexSourceMode>);

fn mode_name(mode: CodexSourceMode) -> &'static str {
    match mode {
        CodexSourceMode::Legacy => "legacy",
        CodexSourceMode::Shadow => "shadow",
        CodexSourceMode::Verified => "verified",
        CodexSourceMode::Invalid => "invalid",
    }
}

impl Drop for SourceMode {
    fn drop(&mut self) {
        SOURCE_MODE_TEST.with(|mode| mode.set(self.0));
    }
}

pub(in crate::services::discord) struct TurnFailureGuard {
    token: Arc<CancelToken>,
    complete: bool,
}

impl TurnFailureGuard {
    pub(in crate::services::discord) fn finish(mut self) {
        assert!(
            self.token.is_completion_cleanup(),
            "only a completed terminal may disarm cleanup"
        );
        assert_eq!(
            self.token.cancel_source(),
            None,
            "an explicit cancel is not completion"
        );
        self.complete = true;
    }
}

impl Drop for TurnFailureGuard {
    fn drop(&mut self) {
        if !self.complete {
            self.token.cancelled.store(true, Ordering::SeqCst);
        }
    }
}

struct Hosting {
    previous: crate::config::Config,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl Hosting {
    fn install(config: &crate::config::Config) -> Self {
        let lock = crate::services::provider_hosting::TEST_CONFIG_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let previous = crate::config_live_reload::current()
            .map(|config| (*config).clone())
            .unwrap_or_default();
        crate::services::provider_hosting::install_provider_hosting_config(config);
        Self {
            previous,
            _lock: lock,
        }
    }
}

impl Drop for Hosting {
    fn drop(&mut self) {
        crate::services::provider_hosting::install_provider_hosting_config(&self.previous);
    }
}

struct Isolation {
    _dedupe: std::sync::MutexGuard<'static, ()>,
    _live: crate::config_live_reload::TestLiveGuard,
}

pub(in crate::services::discord) struct CanaryFixture {
    pub(in crate::services::discord) fake: FakeTmux,
    pub(in crate::services::discord) shared: Arc<SharedData>,
    pub(in crate::services::discord) context: BindingContext,
    pub(in crate::services::discord) old_source: p5::SourceId,
    alive: Arc<AtomicBool>,
    pub(in crate::services::discord) root: tempfile::TempDir,
    _confirmation: TestConfirmation,
    _mode: SourceMode,
    _database: TestPostgresDb,
    _hosts: crate::config::session_hosts::ForcedSessionHosts,
    _env: [crate::config::TestEnvVarGuard; 2],
    _source_env: crate::config::TestEnvVarGuard,
    _host_env: Option<crate::config::TestEnvVarGuard>,
    _binding_env: crate::config::TestEnvVarGuard,
    _hosting: Hosting,
    _isolation: Isolation,
}

impl CanaryFixture {
    pub(in crate::services::discord) async fn new() -> Self {
        Self::new_with_audit_prefix(0).await
    }

    pub(in crate::services::discord) async fn new_with_audit_prefix(prefix: u64) -> Self {
        Self::build(prefix, CodexSourceMode::Verified).await
    }

    /// A live canary whose pin is verified while this process runs in `process_mode`.
    pub(in crate::services::discord) async fn new_in(process_mode: CodexSourceMode) -> Self {
        Self::build(0, process_mode).await
    }

    async fn build(prefix: u64, process_mode: CodexSourceMode) -> Self {
        Self::build_with_host(prefix, process_mode, None).await
    }

    pub(in crate::services::discord) async fn build_with_host(
        prefix: u64,
        process_mode: CodexSourceMode,
        host: Option<&str>,
    ) -> Self {
        let environment = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let live = crate::config_live_reload::test_live_guard(environment);
        let host_env = host.map(|host| {
            crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
                "AGENTDESK_INSTANCE_ID",
                std::ffi::OsStr::new(host),
            )
        });
        let dedupe_lock = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let isolation = Isolation {
            _dedupe: dedupe_lock,
            _live: live,
        };
        let (root, env) = dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
        let hosts = crate::config::session_hosts::force_for_test(None, &[]);
        let workspace = serde_json::to_string(root.path()).unwrap();
        let yaml = format!(
            "server: {{}}\ndiscord:\n  bots:\n    codex: {{token: '{TEST_BOT}', provider: codex}}\nagents:\n  - id: clear-canary\n    name: clear-canary\n    provider: codex\n    channels:\n      codex: {{id: '{CANARY_CHANNEL}', name: adk-codex-tui-e2e, workspace: {workspace}, runtime: tui}}\n"
        );
        let mut config: crate::config::Config = serde_yaml::from_str(&yaml).unwrap();
        config.session_hosts =
            crate::config::session_hosts::with_boot(|boot| boot.unwrap().config().clone());
        fs::write(root.path().join("config.yaml"), &yaml).unwrap();
        let disk = crate::runtime_layout::config_file_path(root.path());
        fs::create_dir_all(disk.parent().unwrap()).unwrap();
        fs::write(disk, &yaml).unwrap();
        let hosting = Hosting::install(&config);
        crate::config_live_reload::install(config);
        assert_eq!(
            crate::services::discord::settings::resolve_workspace(
                serenity::ChannelId::new(CANARY_CHANNEL),
                None,
            ),
            Some(root.path().display().to_string()),
            "boot without a core session resolves the configured workspace",
        );
        assert_eq!(
            crate::services::provider_hosting::resolve_provider_session_selection_with_channel(
                &ProviderKind::Codex,
                true,
                Some(CANARY_CHANNEL),
            )
            .driver,
            crate::services::provider_hosting::ProviderSessionDriver::TuiHosting,
            "the fixture must execute the real direct TUI launch rather than a wrapper",
        );
        dedupe::reset_state_for_tests();
        p5::set_test_root(Some(root.path()));
        let binding_env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_TEST_CODEX_BINDING_ROOT",
            root.path(),
        );
        let mode = SourceMode(SOURCE_MODE_TEST.with(|mode| mode.replace(Some(process_mode))));
        let source_env = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
            "AGENTDESK_CODEX_DIRECT_TUI_SOURCE_MODE",
            mode_name(process_mode).as_ref(),
        );
        let fake = FakeTmux::install(CANARY_TMUX);
        fake.become_missing_after_kill();
        let sessions = root.path().join("sessions");
        fs::create_dir(&sessions).unwrap();
        let context = BindingContext {
            schema: 1,
            provider: "codex".into(),
            created_at: chrono::Utc::now(),
            execution_nonce: uuid::Uuid::new_v4().simple().to_string(),
            tmux_session: CANARY_TMUX.into(),
            channel_id: Some(CANARY_CHANNEL),
            owner_runtime_root: crate::services::tmux_common::current_tmux_owner_marker(),
            host: host.map(str::to_owned),
            expected_native_session_id: None,
            launch_mode: "fresh".into(),
            provider_root: Some(sessions.canonicalize().unwrap()),
            first_prompt_digest: Some(format!("sha256:{:x}", Sha256::digest(PROMPT.as_bytes()))),
            source_policy: Some("verified".into()),
        };
        if prefix > 0 {
            let directory = root.path().join(p5::BINDING_EVENTS_DIR);
            fs::create_dir_all(&directory).unwrap();
            let mut history = String::new();
            for seq in 1..=prefix {
                let record = p5::BindingEvent {
                    seq,
                    channel_id: CANARY_CHANNEL,
                    provider: "codex".into(),
                    tmux_session: CANARY_TMUX.into(),
                    execution_nonce: None,
                    old: None,
                    new: p5::BindingTarget::Rejected {
                        payload_session_id: format!("legacy-audit-{seq}"),
                        payload_transcript_path: None,
                        reason: "legacy audit only".into(),
                    },
                    cause: p5::BindingCause::Unknown,
                    parent_hint: None,
                    evidence: p5::BindingEvidence {
                        hook_event: None,
                        received_at: context.created_at,
                    },
                    committed_at: context.created_at,
                };
                history.push_str(&serde_json::to_string(&record).unwrap());
                history.push('\n');
            }
            fs::write(directory.join(format!("{CANARY_CHANNEL}.log")), history).unwrap();
        }
        let prepared = PreparedIncarnation::create(context.clone()).unwrap();
        crate::services::discord::stamp_spawn_markers(CANARY_TMUX, Some(&prepared)).unwrap();
        dedupe::register_tmux_channel(CANARY_TMUX, CANARY_CHANNEL);
        let native = context
            .provider_root
            .as_ref()
            .unwrap()
            .join(format!("rollout-{NATIVE_ID}.jsonl"));
        let at = (context.created_at + chrono::Duration::seconds(1)).to_rfc3339();
        let metadata = json!({"type":"session_meta", "timestamp":at, "payload":{
            "id":NATIVE_ID,"timestamp":at,"cwd":root.path(),
            "source":"cli","originator":"codex_cli_rs"}});
        fs::write(&native, format!("{metadata}\n")).unwrap();
        let envelope = HookBindingEnvelope {
            context: CapturedContext::Captured(context.clone()),
            observed: ObservedHookProcess::default(),
        };
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(BINDING_HEADER, envelope.encode().unwrap().parse().unwrap());
        let payload = json!({"session_id":NATIVE_ID,"transcript_path":native,
            "source":"startup","prompt":PROMPT});
        assert_eq!(
            observe_binding_hook(
                "codex",
                "session-start",
                Some(NATIVE_ID),
                Some(NATIVE_ID),
                &payload,
                &headers,
            ),
            IngressOutcome::Durable(DurableKind::Adopted),
            "the fixture must create its proof through real hook ingress"
        );
        let proof = p5::codex::read_ownership(&context).unwrap();
        assert!(proof.pending.is_empty() && !proof.conflicted);
        let verified = proof.verified.expect("durable verified proof");
        assert_eq!(verified.seq, prefix + 1);
        let old_source = verified.source;
        let database = TestPostgresDb::create().await;
        let pool = database.connect_and_migrate().await;
        let mut shared =
            crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool));
        Arc::get_mut(&mut shared).unwrap().provider = ProviderKind::Codex;
        shared.settings.write().await.provider = ProviderKind::Codex;
        let channel = serenity::ChannelId::new(CANARY_CHANNEL);
        clear_persist_failure_tests::seed_session(&shared, channel).await;
        {
            let mut core = shared.core.lock().await;
            let session = core.sessions.get_mut(&channel).unwrap();
            session.channel_name = Some("adk-codex-tui-e2e".into());
            session.session_id = Some(NATIVE_ID.into());
            session.current_path = Some(root.path().display().to_string());
        }
        let key = crate::services::discord::adk_session::build_namespaced_session_key(
            &shared.token_hash,
            &ProviderKind::Codex,
            CANARY_TMUX,
        );
        let channel_text = CANARY_CHANNEL.to_string();
        upsert_hook_session_with_identity_pg(
            shared.pg_pool.as_ref().unwrap(),
            HookSessionUpsert {
                session_key: &key,
                instance_id: Some("clear-canary"),
                agent_id: None,
                provider: "codex",
                status: "idle",
                session_info: None,
                model: None,
                tokens: None,
                cwd: None,
                active_dispatch_id: None,
                thread_channel_id: None,
                channel_id: Some(&channel_text),
                claude_session_id: None,
                raw_provider_session_id: Some(NATIVE_ID),
                turn_start_nonce: None,
                dispatched_origin: false,
            },
            Some(CanonicalSessionIdentity {
                kind: SessionIdentityKind::DiscordChannel,
                discord_token_hash: &shared.token_hash,
                channel_id: &channel_text,
            }),
        )
        .await
        .unwrap();
        let alive = Arc::new(AtomicBool::new(true));
        crate::services::session_backend::insert_process_session(
            CANARY_TMUX,
            crate::services::session_backend::SessionHandle::TestProcess {
                pid: 6_845_051,
                alive: alive.clone(),
            },
        );
        Self {
            fake,
            shared,
            context,
            old_source,
            alive,
            root,
            _confirmation: TestConfirmation::new(CANARY_CHANNEL),
            _mode: mode,
            _database: database,
            _hosts: hosts,
            _env: env,
            _source_env: source_env,
            _host_env: host_env,
            _binding_env: binding_env,
            _hosting: hosting,
            _isolation: isolation,
        }
    }

    fn warm_followup(&self, enabled: bool) -> crate::config::TestEnvVarGuard {
        crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
            "AGENTDESK_CODEX_TUI_WARM_FOLLOWUP",
            std::ffi::OsStr::new(if enabled { "1" } else { "0" }),
        )
    }

    pub(in crate::services::discord) fn scoped_test_env(
        &self,
        key: &'static str,
        value: &std::ffi::OsStr,
    ) -> crate::config::TestEnvVarGuard {
        crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(key, value)
    }

    pub(in crate::services::discord) fn history(&self) -> PathBuf {
        self.root
            .path()
            .join(p5::BINDING_EVENTS_DIR)
            .join(format!("{CANARY_CHANNEL}.log"))
    }

    pub(in crate::services::discord) fn cold_cli(&self) -> Vec<crate::config::TestEnvVarGuard> {
        self.fake.enable_fixture_launch();
        let cli = self.root.path().join("codex-fixture");
        fs::write(&cli, format!(r#"#!/bin/sh
if [ "${{1-}}" = --version ]; then echo 'codex-cli 0.157.1'; exit 0; fi
if [ "${{1-}} ${{2-}}" = 'resume --help' ]; then echo '--dangerously-bypass-hook-trust'; exit 0; fi
python3 - "$@" <<'PY'
import datetime, json, os, pathlib, sys
context_path = pathlib.Path(os.environ['AGENTDESK_BINDING_CONTEXT'])
context = json.loads(context_path.read_text())
root = pathlib.Path(context['provider_root']).parent
stamp = datetime.datetime.now(datetime.timezone.utc).isoformat()
identity = '{NEW_NATIVE_ID}'
rows = [
 {{'type':'session_meta','timestamp':stamp,'payload':{{'id':identity,'timestamp':stamp,'cwd':str(root),'source':'cli','originator':'codex_cli_rs'}}}},
 {{'type':'event_msg','payload':{{'type':'task_started','turn_id':'new-clear-turn'}}}},
]
path = pathlib.Path(context['provider_root']) / ('rollout-' + identity + '.jsonl')
path.write_text(''.join(json.dumps(row) + '\n' for row in rows))
(root / 'fresh-argv.json').write_text(json.dumps(sys.argv[1:]))
# The test polls fresh-context, so it must never be visible half-written.
fresh = root / 'fresh-context.tmp'
fresh.write_text(str(context_path))
os.replace(fresh, root / 'fresh-context')
PY
"#)).unwrap();
        fs::set_permissions(&cli, fs::Permissions::from_mode(0o700)).unwrap();
        let set = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock;
        vec![
            set("AGENTDESK_CODEX_PATH", &cli),
            set("CODEX_HOME", self.root.path()),
        ]
    }

    pub(in crate::services::discord) async fn turn_failure_guard(&self) -> TurnFailureGuard {
        let token = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(token) = self
                    .shared
                    .mailbox(serenity::ChannelId::new(CANARY_CHANNEL))
                    .snapshot()
                    .await
                    .cancel_token
                {
                    break token;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the launched turn did not publish its cancellation owner");
        TurnFailureGuard {
            token,
            complete: false,
        }
    }

    pub(in crate::services::discord) async fn wait_new_context(&self) -> BindingContext {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Ok(path) = fs::read_to_string(self.root.path().join("fresh-context")) {
                    let context: BindingContext = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
                    if crate::services::tui_prompt_dedupe::binding_context::observe_spawn_nonce_marker(CANARY_TMUX)
                        == SpawnNonceMarker::Known(context.execution_nonce.clone()) {
                        eprintln!("E51_LAUNCH_STAMP nonce={} root={:?}", context.execution_nonce, context.provider_root);
                        break context;
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.expect("the actual cold launch did not publish its fresh nonce")
    }

    async fn publish_context(
        &self,
        context: BindingContext,
        api: &crate::services::discord::admin_host_guard::tests::Recorder,
        split_tick: bool,
        observed: &mut Vec<String>,
    ) -> BindingContext {
        let path = context
            .provider_root
            .as_ref()
            .unwrap()
            .join(format!("rollout-{NEW_NATIVE_ID}.jsonl"));
        let envelope = HookBindingEnvelope {
            context: CapturedContext::Captured(context.clone()),
            observed: ObservedHookProcess::default(),
        };
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(BINDING_HEADER, envelope.encode().unwrap().parse().unwrap());
        let outcome = observe_binding_hook(
            "codex",
            "session-start",
            Some(NEW_NATIVE_ID),
            Some(NEW_NATIVE_ID),
            &json!({"session_id":NEW_NATIVE_ID,"transcript_path":path,"source":"startup"}),
            &headers,
        );
        // Only a verified launch records ownership; the others keep the legacy publication.
        if context.source_policy.as_deref() == Some("verified") {
            assert_eq!(outcome, IngressOutcome::Durable(DurableKind::Adopted));
        }
        let fold = p5::codex::read_ownership(&context).ok();
        let binding = dedupe::runtime_binding_for_tmux_session(CANARY_TMUX);
        let allowed = dedupe::codex_verified_source_allowed(
            CANARY_TMUX,
            &path.display().to_string(),
            Some(NEW_NATIVE_ID),
        );
        eprintln!(
            "E51_HOOK_PUBLISHED nonce={} proof={:?} pending={:?} binding={:?} source_allowed={allowed}",
            context.execution_nonce,
            fold.as_ref()
                .and_then(|fold| fold.verified.as_ref().map(|p| p.seq)),
            fold.as_ref().map(|fold| fold.pending.len()),
            binding,
        );
        assert!(
            allowed,
            "the actual verified consumer must admit its new proof"
        );
        let mut output = fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(
            output,
            "{}",
            json!({"type":"response_item","payload":{"type":"message","role":"assistant",
            "content":[{"type":"output_text","text":"clear relaunch completed"}]}})
        )
        .unwrap();
        output.flush().unwrap();
        if split_tick {
            tokio::time::timeout(Duration::from_secs(20), async {
                loop {
                    observed.extend(api.take());
                    let seen = observed.iter().any(|call| {
                        let Some(body) = call.strip_prefix(&format!(
                            "PATCH /api/v10/channels/{CANARY_CHANNEL}/messages/900001 "
                        )) else {
                            return false;
                        };
                        serde_json::from_str::<serde_json::Value>(body).unwrap()["content"]
                            == "clear relaunch completed"
                    });
                    if seen {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("split tick must observe the exact body PATCH before Done");
            eprintln!("E51_SPLIT_TICK exact body PATCH observed before Done");
        }
        for row in [
            json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"new-clear-turn",
                "last_agent_message":"clear relaunch completed"}}),
            json!({"type":"event_msg","payload":{"type":"composer_ready","turn_id":"new-clear-turn"}}),
        ] {
            writeln!(output, "{row}").unwrap();
        }
        output.flush().unwrap();
        context
    }
}

impl Drop for CanaryFixture {
    fn drop(&mut self) {
        let _ = crate::services::session_backend::remove_process_session(CANARY_TMUX);
        p5::forget_channel_for_tests(CANARY_CHANNEL);
        p5::set_test_root(None);
        dedupe::reset_state_for_tests();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn canary_clear_reopens_launch_without_releasing_old_source_pg() {
    clear_and_first_input(
        concat!(
            module_path!(),
            "::canary_clear_reopens_launch_without_releasing_old_source_pg"
        ),
        (false, false),
        CodexSourceMode::Verified,
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn canary_clear_with_unwatched_external_tmux_launches_first_input_pg() {
    clear_and_first_input(
        concat!(
            module_path!(),
            "::canary_clear_with_unwatched_external_tmux_launches_first_input_pg"
        ),
        (true, false),
        CodexSourceMode::Verified,
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn clear_without_core_session_still_retires_dead_canary_pg() {
    clear_and_first_input(
        concat!(
            module_path!(),
            "::clear_without_core_session_still_retires_dead_canary_pg"
        ),
        (true, true),
        CodexSourceMode::Verified,
    )
    .await;
}

/// A verified pin outlives a rollback of the process mode, so the clear must reopen launch there.
#[tokio::test(flavor = "current_thread")]
async fn shadow_canary_clear_launches_the_first_input_pg() {
    let test = concat!(
        module_path!(),
        "::shadow_canary_clear_launches_the_first_input_pg"
    );
    clear_and_first_input(test, (false, false), CodexSourceMode::Shadow).await;
}

#[tokio::test(flavor = "current_thread")]
async fn shadow_canary_clear_without_core_session_launches_the_first_input_pg() {
    let test = concat!(
        module_path!(),
        "::shadow_canary_clear_without_core_session_launches_the_first_input_pg"
    );
    clear_and_first_input(test, (true, true), CodexSourceMode::Shadow).await;
}

#[tokio::test(flavor = "current_thread")]
async fn legacy_canary_clear_launches_the_first_input_pg() {
    let test = concat!(
        module_path!(),
        "::legacy_canary_clear_launches_the_first_input_pg"
    );
    clear_and_first_input(test, (false, false), CodexSourceMode::Legacy).await;
}

#[tokio::test(flavor = "current_thread")]
async fn legacy_canary_clear_without_core_session_launches_the_first_input_pg() {
    let test = concat!(
        module_path!(),
        "::legacy_canary_clear_without_core_session_launches_the_first_input_pg"
    );
    clear_and_first_input(test, (true, true), CodexSourceMode::Legacy).await;
}

/// Clears the pinned canary, sends the first input through intake and clears the relaunch again.
async fn clear_and_first_input(
    test: &str,
    (external_inventory, without_core): (bool, bool),
    process_mode: CodexSourceMode,
) {
    if !crate::services::tui_o::cutover::test_override::in_empty_list_process(test) {
        return;
    }
    let qualified = test.split_once("::").unwrap().1;
    if !crate::services::discord::admin_host_guard::tests::api_child(qualified) {
        return;
    }
    for split_tick in [true, false] {
        eprintln!("E51_TICK_VARIANT split_tick={split_tick}");
        clear_and_first_input_at_tick(external_inventory, without_core, process_mode, split_tick)
            .await;
    }
}

pub(in crate::services::discord) async fn settled_owner_observation<T, F, Fut>(
    counter: &str,
    mut observe: F,
    transient: impl Fn(&T) -> bool,
) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let mut retries = 0;
    let observed = tokio::time::timeout(Duration::from_secs(5), async {
        for read in 0..50 {
            let sample = observe().await;
            if !transient(&sample) {
                return Some(sample);
            }
            retries += 1;
            if read + 1 < 50 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
        None
    })
    .await;
    eprintln!("{counter}={retries}");
    observed
        .expect("owner observation remained transient past the bounded reread timeout")
        .expect("owner observation remained transient through the bounded reread limit")
}

pub(in crate::services::discord) fn owner_epoch_error<T, E: AsRef<str>>(
    observed: &Result<T, E>,
) -> bool {
    matches!(observed, Err(reason) if reason.as_ref() == "owner_epoch")
}

#[tokio::test]
async fn owner_epoch_retry_keeps_unrelated_error_on_first_observation() {
    let calls = std::cell::Cell::new(0);
    let observed = settled_owner_observation(
        "OWNER_RETRY_CONTRACT unrelated_error_retries",
        || {
            let call = calls.get();
            calls.set(call + 1);
            std::future::ready(if call == 0 {
                Err("fixture_io".to_owned())
            } else {
                Ok("Hold(\"owner_unresolved\")".to_owned())
            })
        },
        owner_epoch_error,
    )
    .await;
    assert_eq!(
        observed,
        Err("fixture_io".to_owned()),
        "a non-owner-epoch error must remain the first observation"
    );
    assert_eq!(calls.get(), 1, "an unrelated error must not be reread");
}

#[tokio::test]
async fn owner_epoch_retry_rechecks_one_transient_before_stable_hold() {
    let calls = std::cell::Cell::new(0);
    let observed = settled_owner_observation(
        "OWNER_RETRY_CONTRACT transient_retries",
        || {
            let call = calls.get();
            calls.set(call + 1);
            std::future::ready(if call == 0 {
                Err("owner_epoch".to_owned())
            } else {
                Ok("Hold(\"owner_unresolved\")".to_owned())
            })
        },
        owner_epoch_error,
    )
    .await;
    assert_eq!(
        observed,
        Ok("Hold(\"owner_unresolved\")".to_owned()),
        "an owner-epoch error must be reread before accepting the observation"
    );
    assert_eq!(calls.get(), 2, "only the transient observation is retried");
}

async fn clear_and_first_input_at_tick(
    external_inventory: bool,
    without_core: bool,
    process_mode: CodexSourceMode,
    split_tick: bool,
) {
    let mut fixture = CanaryFixture::new_in(process_mode).await;
    assert!(
        !crate::services::tui_o::cutover::peek_o_owns_tui_output_for_channel(
            CANARY_CHANNEL,
            Some(crate::services::agent_protocol::RuntimeHandoffKind::CodexTui),
        )
        .expect("the fixture installs an explicit empty O boot list")
    );
    let api = crate::services::discord::admin_host_guard::tests::Recorder::start_with(Arc::new(
        |method, path| {
            (method == axum::http::Method::GET
                && path.ends_with(&format!("/channels/{CANARY_CHANNEL}")))
            .then(|| {
                json!({
                    "id": CANARY_CHANNEL.to_string(), "type": 0,
                    "name": "adk-codex-tui-e2e", "guild_id": "42000",
                    "position": 0, "permission_overwrites": [], "nsfw": false,
                    "parent_id": null
                })
            })
        },
    ))
    .await;
    crate::services::discord::internal_api::init(api.port, None);
    Arc::get_mut(&mut fixture.shared).unwrap().api_port = api.port;
    let _hook = crate::services::claude_tui::hook_server::publish_hook_endpoint(format!(
        "http://127.0.0.1:{}",
        api.port
    ));
    let _cli = fixture.cold_cli();
    let channel = serenity::ChannelId::new(CANARY_CHANNEL);
    assert!(crate::services::tui_o::turn_mode::transcript_turns(
        CANARY_CHANNEL
    ));
    assert!(dedupe::codex_verified_requires_proof(CANARY_TMUX));
    assert!(!dedupe::codex_verified_input_blocked(CANARY_TMUX));
    let before = fs::read(fixture.history()).unwrap();
    if external_inventory {
        fixture
            .fake
            .add_unwatched_inventory("AgentDesk-codex-external-e51-unwatched");
    }
    if without_core {
        fixture.shared.core.lock().await.sessions.remove(&channel);
        fixture.fake.mark_missing();
        fixture.alive.store(false, Ordering::SeqCst);
        crate::services::session_backend::remove_process_session(CANARY_TMUX);
        assert!(!dedupe::clear_relaunch::relaunch_open(CANARY_TMUX));
    }
    clear_persist_failure_tests::seed_backlog(&fixture.shared, &ProviderKind::Codex, channel).await;
    let cleared = clear_channel_session_state(
        &api.http,
        &fixture.shared,
        &ProviderKind::Codex,
        channel,
        "!clear",
    )
    .await
    .unwrap();
    assert_no_retirement_warning(&cleared);
    assert!(!fixture.alive.load(Ordering::SeqCst));
    let kills = fixture
        .fake
        .take_calls()
        .iter()
        .filter(|call| call.starts_with("kill-session"))
        .count();
    assert_eq!(
        kills,
        usize::from(!without_core),
        "only the core-selected session is ended"
    );
    assert!(
        dedupe::clear_relaunch::relaunch_open(CANARY_TMUX),
        "an explicit clear must retire the dead canary even without a core session"
    );
    let watermark: serde_json::Value = serde_json::from_slice(
        &fs::read(
            fixture
                .root
                .path()
                .join("runtime/codex_clear_retired")
                .join(format!("{CANARY_CHANNEL}.json")),
        )
        .expect("explicit clear creates a durable retirement watermark"),
    )
    .unwrap();
    assert_eq!(watermark["origin"], "explicit_clear");
    assert_eq!(
        crate::services::tui_prompt_dedupe::binding_context::observe_spawn_nonce_marker(
            CANARY_TMUX
        ),
        SpawnNonceMarker::Absent,
    );
    assert_eq!(
        fs::read(fixture.history()).unwrap(),
        before,
        "old proof history is retained"
    );
    assert!(
        p5::codex::read_ownership(&fixture.context)
            .unwrap()
            .verified
            .is_some()
    );
    assert!(!dedupe::codex_verified_o_source_allowed(
        CANARY_CHANNEL,
        &fixture.old_source
    ));
    assert!(dedupe::codex_verified_requires_proof(CANARY_TMUX));
    assert!(dedupe::codex_verified_input_blocked(CANARY_TMUX));
    assert_eq!(
        clear_persist_failure_tests::queue_len(&fixture.shared, channel).await,
        0
    );
    let observed = settled_owner_observation(
        "E51_AFTER_CLEAR superseded_retries",
        || activity::activity_now(&fixture.shared, &ProviderKind::Codex, channel),
        |sample| sample.reason == "superseded",
    )
    .await;
    assert_eq!(
        (observed.activity, observed.reason),
        if external_inventory {
            (Activity::Unknown, "session_identity_unresolved")
        } else {
            (Activity::Idle, "no_session")
        }
    );
    let cancelled_probe = Arc::new(CancelToken::new());
    cancelled_probe.cancelled.store(true, Ordering::SeqCst);
    let clear_preflight_open =
        crate::services::codex_tui::verified_hold::preflight(CANARY_TMUX, Some(&cancelled_probe))
            .is_none();
    let underlying = settled_owner_observation(
        "E51_AFTER_CLEAR underlying_owner_epoch_retries",
        || {
            activity::tests::unbound_verdict_for_tests(
                &fixture.shared,
                &ProviderKind::Codex,
                channel,
            )
        },
        owner_epoch_error,
    )
    .await;
    eprintln!(
        "E51_AFTER_CLEAR activity={:?} reason={} underlying={underlying:?}",
        observed.activity, observed.reason
    );
    let request = crate::services::discord::router::message_handler::IntakeRequest {
        intake_outbox_id: None,
        channel_id: channel,
        user_msg_id: serenity::MessageId::new(CANARY_CHANNEL + 51),
        source_message_ids: Vec::new(),
        busy_followup_retry_user_msg_id: serenity::MessageId::new(CANARY_CHANNEL + 51),
        request_owner: serenity::UserId::new(7),
        request_owner_name: "fixture owner".into(),
        user_text: AFTER_CLEAR.into(),
        reply_to_user_message: false,
        defer_watcher_resume: false,
        wait_for_completion: true,
        merge_consecutive: false,
        reply_context: None,
        has_reply_boundary: false,
        dm_hint: Some(false),
        turn_kind: crate::services::discord::router::TurnKind::Foreground,
        preserve_on_cancel: false,
    };
    let mut failure_guard = None;
    let mut before_done = Vec::new();
    let (result, events) = crate::services::observability::events::test_capture::capture_async(
        tokio::time::timeout(Duration::from_secs(30), async {
            let intake =
                crate::services::discord::router::message_handler::execute_intake_turn_core(
                    &api.http,
                    &fixture.shared,
                    TEST_BOT,
                    request,
                    Vec::new(),
                );
            let hook = async {
                let guard = fixture.turn_failure_guard().await;
                let token = guard.token.clone();
                failure_guard = Some(guard);
                let new = fixture.wait_new_context().await;
                (
                    fixture
                        .publish_context(new, &api, split_tick, &mut before_done)
                        .await,
                    token,
                )
            };
            tokio::pin!(intake, hook);
            tokio::select! {
                // On main this returns the real queued_for_retry verdict before any spawn.
                completed = &mut intake => (completed, None),
                (new, token) = &mut hook => {
                    let completed = intake.await;
                    assert!(token.is_completion_cleanup(), "the actual terminal did not complete");
                    assert_eq!(token.cancel_source(), None, "the turn completed by cancellation");
                    (completed, Some(new))
                }
            }
        }),
    )
    .await;
    let (completed, new) = result.expect("the actual intake did not complete its terminal turn");
    completed.unwrap();
    let busy = events
        .iter()
        .find(|event| event.payload["kind"] == "claude_tui_followup_busy_pre_submit");
    let calls = fixture.fake.take_calls();
    let launches = calls
        .iter()
        .filter(|call| call.starts_with("new-session "))
        .count();
    let depth = clear_persist_failure_tests::queue_len(&fixture.shared, channel).await;
    eprintln!(
        "E51_FIRST_INPUT launches={launches} queue_depth={depth} busy={:?}",
        busy.map(|event| &event.payload["extra"])
    );
    assert!(
        busy.is_none(),
        "first input was queued_for_retry instead of reaching a fresh launch: launches={launches}, depth={depth}, diagnostic={:?}",
        busy.map(|event| &event.payload["extra"])
    );
    assert!(
        clear_preflight_open,
        "clear opens only the cold-launch preflight"
    );
    if external_inventory {
        assert_eq!(underlying.unwrap(), "Hold(\"owner_unresolved\")");
    } else {
        assert_eq!(underlying.unwrap(), "Absent(LocalInventory)");
    }
    let new = new.unwrap_or_else(|| {
        panic!(
            "terminal completion preceded a real fresh hook proof; API calls={:?}; launch calls={calls:?}",
            api.take(),
        )
    });
    assert_ne!(new.execution_nonce, fixture.context.execution_nonce);
    assert_eq!(new.launch_mode, "fresh");
    let verified = process_mode == CodexSourceMode::Verified;
    assert_eq!(new.source_policy.as_deref(), Some(mode_name(process_mode)));
    assert!(new.expected_native_session_id.is_none());
    let argv: Vec<String> =
        serde_json::from_slice(&fs::read(fixture.root.path().join("fresh-argv.json")).unwrap())
            .unwrap();
    assert!(!argv.iter().any(|arg| arg == "resume" || arg == NATIVE_ID));
    let calls = calls
        .into_iter()
        .chain(fixture.fake.take_calls())
        .collect::<Vec<_>>();
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.starts_with("new-session "))
            .count(),
        1
    );
    assert_eq!(
        clear_persist_failure_tests::queue_len(&fixture.shared, channel).await,
        0
    );
    if verified {
        let current = p5::codex::read_ownership(&new).unwrap();
        assert_eq!(current.verified.unwrap().source.session_id, NEW_NATIVE_ID);
        assert!(current.pending.is_empty() && !current.conflicted);
    }
    // A cancelled probe returns immediately even if a mutant restores the old blocking hold.
    let cancel = Arc::new(CancelToken::new());
    cancel.cancelled.store(true, Ordering::SeqCst);
    assert_eq!(
        crate::services::codex_tui::verified_hold::preflight(CANARY_TMUX, Some(&cancel)).is_some(),
        verified,
        "only a new verified incarnation closes the launch allowance"
    );
    let after = fixture.shared.mailbox(channel).snapshot().await;
    assert!(
        after.cancel_token.is_none(),
        "terminal completion released the turn"
    );
    assert!(after.pending_user_dispatch.is_none());
    assert!(after.intervention_queue.is_empty());
    before_done.extend(api.take());
    assert_one_completed_response(&before_done);
    failure_guard
        .expect("the actual turn had a cancellation owner")
        .finish();
    let again = clear_channel_session_state(
        &api.http,
        &fixture.shared,
        &ProviderKind::Codex,
        channel,
        "!clear",
    )
    .await
    .unwrap();
    assert_no_retirement_warning(&again);
    assert!(
        dedupe::clear_relaunch::relaunch_open(CANARY_TMUX),
        "clearing the relaunched incarnation reopens launch again"
    );
}

/// The clear retired its canary: no retirement step failed, so its reply carries no such warning.
fn assert_no_retirement_warning(report: &ClearReport) {
    let retirement = |what: &&str| what.starts_with("codex_retirement");
    assert!(!report.failed.iter().any(retirement), "{report:?}");
    eprintln!("E51_CLEAR_REPLY {:?}", report.reply());
}

pub(in crate::services::discord) fn assert_one_completed_response(sent: &[String]) {
    const RESPONSE: &str = "clear relaunch completed";
    let mut body_calls = Vec::new();
    let mut summaries = Vec::new();
    let mut posts = 0;
    let mut messages = std::collections::BTreeSet::new();
    for (index, call) in sent.iter().enumerate() {
        let mut parts = call.splitn(3, ' ');
        let method = parts.next().unwrap_or_default();
        let path = parts.next().unwrap_or_default();
        let body = parts.next().unwrap_or_default();
        if !matches!(method, "POST" | "PATCH")
            || !path.contains(&format!("/channels/{CANARY_CHANNEL}/messages"))
        {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(body).expect("message payload");
        let content = value["content"].as_str().expect("message content");
        if content.starts_with("-# ") {
            assert!(
                method == "POST" && content.starts_with("-# ✅ "),
                "completion summary: {sent:?}"
            );
            summaries.push(index);
            continue;
        }
        let target = if method == "POST" {
            posts += 1;
            format!("{path}/900001")
        } else {
            path.to_owned()
        };
        messages.insert(target.clone());
        body_calls.push((index, method, target, content.to_owned()));
    }
    assert_eq!(
        messages.len(),
        1,
        "one response message including progress edits: {sent:?}"
    );
    assert!(posts <= 1, "a repeated POST is another response: {sent:?}");
    let last = body_calls.last().expect("completed body call");
    assert_eq!(
        last.3, RESPONSE,
        "last body call settles the response: {sent:?}"
    );
    let finals = body_calls.iter().filter(|call| call.3 == RESPONSE).count();
    assert!(
        (1..=2).contains(&finals),
        "one settle and at most one identical streaming edit: {sent:?}"
    );
    for call in &body_calls[..body_calls.len() - 1] {
        if call.3 == RESPONSE {
            assert!(
                call.1 == "PATCH" && call.2 == last.2,
                "earlier exact body only edits the same target: {sent:?}"
            );
        }
    }
    assert_eq!(summaries.len(), 1, "one completed turn summary: {sent:?}");
    assert!(
        summaries[0] > last.0,
        "summary follows the last body call: {sent:?}"
    );
    eprintln!(
        "E51_SETTLED finals={finals} summaries=1 last_body={} summary={}",
        last.0, summaries[0]
    );
}
