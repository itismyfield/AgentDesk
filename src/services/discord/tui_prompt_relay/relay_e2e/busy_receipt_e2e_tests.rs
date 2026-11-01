use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::tui_input::human_frame::{self, history};
use serde_json::{Value, json};

const QUEUED: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/tui_input/claude-2.1.316-queued.jsonl"
));
const ABSORBED: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/tui_input/claude-2.1.316-absorbed.jsonl"
));
const TYPED: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/tui_input/claude-2.1.316-typed.jsonl"
));
const BODY: &str = "BUSY_RECEIPT_RESPONSE";

fn run(body: impl std::future::Future<Output = ()>) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(Box::pin(body));
}

fn isolated(name: &str) -> bool {
    if std::env::var("ADK_BUSY_RECEIPT_CHILD").as_deref() == Ok(name) {
        if std::env::var_os("ADK_BUSY_RECEIPT_TRACE").is_some() {
            tracing_subscriber::fmt()
                .with_ansi(false)
                .without_time()
                .with_max_level(tracing::Level::DEBUG)
                .with_writer(std::io::stderr)
                .try_init()
                .unwrap();
        }
        return true;
    }
    use std::os::unix::fs::PermissionsExt;
    let fake = tempfile::tempdir().unwrap();
    let tmux = fake.path().join("tmux");
    std::fs::write(&tmux, "#!/bin/sh\nwhile [ \"${1#-}\" != \"$1\" ]; do shift; done\ncase \"$1\" in has-session) exit 0;; list-panes) echo 0; exit 0;; capture-pane) echo pane; exit 0;; esac\nexit 1\n").unwrap();
    std::fs::set_permissions(&tmux, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut paths = vec![fake.path().to_path_buf()];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let module = module_path!().split_once("::").unwrap().1;
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &format!("{module}::{name}"), "--nocapture"])
        .env("PATH", std::env::join_paths(paths).unwrap())
        .env("ADK_BUSY_RECEIPT_CHILD", name)
        .env(
            crate::services::tui_o::cutover::test_override::CHANNELS_ENV,
            "[]",
        )
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    print!("{stdout}");
    assert!(
        output.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("1 passed; 0 failed; 0 ignored"), "{stdout}");
    false
}

fn rows(fixture: &str) -> Vec<Value> {
    fixture
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect()
}

fn frame() -> String {
    rows(QUEUED).last().unwrap()["message"]["content"]
        .as_str()
        .unwrap()
        .into()
}

fn transcript(fixture: &str) -> String {
    let mut result = fixture.to_owned();
    let parent = rows(fixture)
        .iter()
        .rev()
        .find(|row| row["type"] == "user")
        .and_then(|row| row["uuid"].as_str())
        .unwrap_or("00000000-0000-4000-8000-000000000004")
        .to_owned();
    result.push_str(&format!("{}\n{}\n{}\n", json!({"type":"assistant", "uuid":"busy-answer-row","parentUuid":parent,"sessionId":"00000000-0000-4000-8000-000000000001","entrypoint":"cli","apiBlockIndex":0, "message":{"id":"busy-answer", "role":"assistant", "content":[{"type":"text","text":BODY}], "stop_reason":"end_turn"}}),json!({"type":"system","subtype":"stop_hook_summary"}),json!({"type":"system","subtype":"turn_duration","durationMs":100})));
    result
}

async fn setup() -> (RelayE2eHarness, std::path::PathBuf) {
    let env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let h = RelayE2eHarness::start_inner_with_env_lock(
        ProviderStub::Success,
        true,
        std::future::ready(None),
        false,
        env_lock,
    )
    .await;
    crate::services::discord::rebind_channel_session(
        &h.shared,
        &ProviderKind::Claude,
        h.channel_id,
        h.root.path().to_str().unwrap(),
        "00000000-0000-4000-8000-000000000001",
    )
    .await;
    h.cache_relay_transport();
    h.answer_placeholders_immediately();
    let registry = h.health_registry.as_ref().unwrap();
    registry
        .set_utility_bot_http_for_tests(
            crate::services::discord::bot_role::UtilityBotRole::Notify,
            h.ctx.http.clone(),
        )
        .await;
    registry
        .register(PROVIDER_KEY.to_string(), h.shared.clone())
        .await;
    let path = h.attach_tmux_watcher(
        "AgentDesk-claude-i7162-fixture",
        "00000000-0000-4000-8000-000000000001.jsonl",
    );
    h.shared
        .tmux_watchers
        .get(&h.channel_id)
        .unwrap()
        .cancel
        .store(true, Ordering::Release);
    dedupe::register_tmux_channel("AgentDesk-claude-i7162-fixture", CHANNEL_ID);
    dedupe::register_tmux_runtime_binding(
        "AgentDesk-claude-i7162-fixture",
        dedupe::TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            output_path: path.display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: Some("00000000-0000-4000-8000-000000000001".into()),
            last_offset: 0,
            relay_last_offset: None,
        },
    );
    (h, path)
}

fn current_rows(fixture: &str) -> String {
    let mut input = rows(fixture);
    let latest = input
        .iter()
        .filter_map(|r| {
            r["timestamp"]
                .as_str()?
                .parse::<chrono::DateTime<chrono::Utc>>()
                .ok()
        })
        .max();
    if let Some(latest) = latest {
        let shift = chrono::Utc::now() - latest;
        for row in &mut input {
            if let Some(at) = row["timestamp"]
                .as_str()
                .and_then(|v| v.parse::<chrono::DateTime<chrono::Utc>>().ok())
            {
                row["timestamp"] = json!((at + shift).to_rfc3339());
            }
        }
    }
    input.iter().map(|r| format!("{r}\n")).collect()
}

struct NativeFixture {
    _events: dedupe::hosted_source::tests::BindingRoot,
    _home: crate::config::TestEnvVarGuard,
    _tmux: crate::services::provider_teardown::tests::test_support::FakeTmux,
    h: RelayE2eHarness,
    path: std::path::PathBuf,
}
impl NativeFixture {
    async fn new() -> Self {
        let (h, _) = setup().await;
        let logical = "AgentDesk-claude-i7162-fixture";
        let tmux =
            crate::services::provider_teardown::tests::test_support::FakeTmux::install(logical);
        let events = dedupe::hosted_source::tests::BindingRoot::at(h.root.path());
        let home = h.root.path().join("claude-home");
        let home_guard = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "CLAUDE_CONFIG_DIR",
            &home,
        );
        let native = "00000000-0000-4000-8000-000000000001";
        let path = crate::services::claude_tui::transcript_tail::claude_transcript_path(
            h.root.path(),
            native,
            None,
        )
        .unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "").unwrap();
        incarnation(&h, native, "11111111111111111111111111111111", &home, false);
        let watcher = watcher_handle(logical, &path);
        watcher.cancel.store(true, Ordering::Release);
        h.shared.tmux_watchers.insert(h.channel_id, watcher);
        let mut binding = dedupe::runtime_binding_for_tmux_session(logical).unwrap();
        binding.output_path = path.display().to_string();
        binding.last_offset = 0;
        dedupe::register_tmux_runtime_binding(logical, binding);
        let generation = std::path::PathBuf::from(crate::services::tmux_common::session_temp_path(
            logical,
            "generation",
        ));
        std::fs::create_dir_all(generation.parent().unwrap()).unwrap();
        std::fs::write(generation, "native fixture generation").unwrap();
        select_o(false);
        let shared = h.shared.clone();
        super::super::hook_observer::spawn_tui_prompt_relay_observer(
            "claude".into(),
            crate::services::claude_tui::hook_server::subscribe_hook_events(),
            move |prompt| {
                let shared = shared.clone();
                Box::pin(async move { super::super::relay_observed_prompt(&shared, prompt).await })
            },
        );
        Self {
            _events: events,
            _home: home_guard,
            _tmux: tmux,
            h,
            path,
        }
    }
}

fn register(path: &std::path::Path, id: &str, prompt: &str) {
    history::register(
        ("claude", "AgentDesk-claude-i7162-fixture", id, "200"),
        path,
        0,
        prompt,
    )
    .unwrap();
}

async fn tick(h: &RelayE2eHarness) -> Vec<dedupe::ObservedTuiPrompt> {
    let candidates = super::super::source_tick::claude_source_phase(&h.shared).await;
    assert!(
        candidates
            .iter()
            .any(
                |candidate| candidate.tmux_session_name == "AgentDesk-claude-i7162-fixture"
                    && candidate.transcript_path.is_some()
            ),
        "no-watcher control must reach a real transcript scanner"
    );
    let mut rx = dedupe::subscribe_observed_prompts();
    super::super::claude_idle_runtime::relay_idle_claude_bindings(&h.shared).await;
    std::iter::from_fn(|| rx.try_recv().ok())
        .filter(|r| r.tmux_session_name == "AgentDesk-claude-i7162-fixture")
        .collect()
}

#[test]
fn legacy_exact_queued_user_reaches_ordinary_once() {
    if !isolated("legacy_exact_queued_user_reaches_ordinary_once") {
        return;
    }
    run(async {
        let native = NativeFixture::new().await;
        let (h, path) = (&native.h, &native.path);

        register(&path, "busy-attempt", &frame());
        std::fs::write(&path, current_rows(&transcript(QUEUED))).unwrap();
        let observed = tick(&h).await;
        assert_eq!(
            observed.len(),
            1,
            "registered queued native user must reach ordinary publication"
        );
        assert!(
            observed[0].hosted_source.is_some(),
            "body fixture must retain its actual launch/source pin"
        );
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if h.messages().iter().any(|(_, s)| s.contains(BODY)) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("ordinary tail must post the B body");
        assert_eq!(
            h.messages()
                .iter()
                .filter(|(_, s)| s.contains(BODY))
                .count(),
            1
        );
        assert_eq!(h.placeholder_posts(), 1);
        assert_eq!(
            body_message_ids(&h).len(),
            1,
            "B was displayed on exactly one message id"
        );
        assert_eq!(
            h.messages().iter().filter(|(_, s)| s.contains('⌨')).count(),
            1
        );
        assert!(tick(&h).await.is_empty());
        assert_eq!(
            h.placeholder_posts(),
            1,
            "replay must create no placeholder"
        );
        until("B tail drained", || {
            !super::super::CLAUDE_IDLE_RESPONSE_TAILS
                .lock()
                .unwrap()
                .contains("AgentDesk-claude-i7162-fixture")
        })
        .await;
        let followup = format!(
            "{}\n{}\n{}\n",
            json!({"type":"user","uuid":"normal-followup-c","timestamp":chrono::Utc::now().to_rfc3339(),"sessionId":"00000000-0000-4000-8000-000000000001","promptSource":"typed","origin":{"kind":"human"},"message":{"role":"user","content":"normal followup C"}}),
            json!({"type":"assistant","uuid":"followup-answer","sessionId":"00000000-0000-4000-8000-000000000001","message":{"id":"followup-answer","role":"assistant","content":[{"type":"text","text":"FOLLOWUP_C_BODY"}],"stop_reason":"end_turn"}}),
            json!({"type":"system","subtype":"stop_hook_summary"})
        );
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(followup.as_bytes())
            .unwrap();
        let followup = tick(&h).await;
        assert_eq!(
            followup.len(),
            1,
            "the normal typed followup retains its own observation"
        );
        assert!(followup[0].hosted_source.is_some());
        until("normal followup C body", || {
            h.messages()
                .iter()
                .any(|(_, s)| s.contains("FOLLOWUP_C_BODY"))
        })
        .await;
        assert_eq!(
            h.messages()
                .iter()
                .filter(|(_, s)| s.contains("FOLLOWUP_C_BODY"))
                .count(),
            1
        );
        assert_eq!(
            h.messages()
                .iter()
                .filter(|(_, s)| s.contains(BODY))
                .count(),
            1
        );
        assert!(
            h.unhandled_requests().is_empty(),
            "{:?}",
            h.unhandled_requests()
        );
        dedupe::clear_tmux_runtime_binding("AgentDesk-claude-i7162-fixture");
    });
}

#[test]
fn legacy_typed_same_frame_and_absorption_do_not_open_turns() {
    if !isolated("legacy_typed_same_frame_and_absorption_do_not_open_turns") {
        return;
    }
    run(async {
        for fixture in [TYPED, ABSORBED] {
            let (h, path) = setup().await;
            register(&path, "busy-attempt", &frame());
            std::fs::write(&path, transcript(fixture)).unwrap();
            assert!(tick(&h).await.is_empty());
            assert_eq!(h.placeholder_posts(), 0);
            assert!(!h.relay_lease_present("AgentDesk-claude-i7162-fixture"));
            dedupe::clear_tmux_runtime_binding("AgentDesk-claude-i7162-fixture");
        }
    });
}

#[test]
fn legacy_unregistered_v1_submit_does_not_open_turn() {
    if !isolated("legacy_unregistered_v1_submit_does_not_open_turn") {
        return;
    }
    run(async {
        let (h, _) = setup().await;
        let prompt = human_frame::inject(
            "discord",
            "200",
            "Fixture User",
            "0000ffff",
            "unvouched busy frame",
        );
        let mut rx = dedupe::subscribe_observed_prompts();
        let got = dedupe::observe_prompt_by_tmux_at(
            "claude",
            "AgentDesk-claude-i7162-fixture",
            &prompt,
            chrono::Utc::now(),
        );
        assert_ne!(
            got,
            dedupe::PromptObservation::PublishedSshDirect,
            "an unregistered v1 hook/pane creates no ordinary owner"
        );
        assert!(rx.try_recv().is_err());
        assert!(!h.relay_lease_present("AgentDesk-claude-i7162-fixture"));
        dedupe::clear_tmux_runtime_binding("AgentDesk-claude-i7162-fixture");
    });
}

#[test]
fn legacy_native_user_requires_queued_human_and_complete_exact_record() {
    if !isolated("legacy_native_user_requires_queued_human_and_complete_exact_record") {
        return;
    }
    run(async {
        for case in [
            "typed",
            "missing-source",
            "notification",
            "nontext",
            "merged",
            "partial",
            "new-inode",
        ] {
            let (h, path) = setup().await;
            register(&path, "busy-attempt", &frame());
            let mut input = rows(QUEUED);
            let user = input.last_mut().unwrap();
            match case {
                "typed" => user["promptSource"] = json!("typed"),
                "missing-source" => {
                    user.as_object_mut().unwrap().remove("promptSource");
                }
                "notification" => user["origin"]["kind"] = json!("notification"),
                "nontext" => {
                    user["message"]["content"] = json!([{"type":"text","text":frame()},{"type":"tool_result","content":"other input"}])
                }
                "merged" => user["message"]["content"] = json!(format!("{}\n{}", frame(), frame())),
                _ => {}
            }
            let mut text = input.iter().map(|r| format!("{r}\n")).collect::<String>();
            if case == "partial" {
                text.pop();
            }
            if case == "new-inode" {
                let replacement = path.with_extension("replacement");
                std::fs::write(&replacement, &text).unwrap();
                std::fs::rename(replacement, &path).unwrap();
            } else {
                std::fs::write(&path, &text).unwrap();
            }
            assert!(
                tick(&h).await.is_empty(),
                "{case} must not vouch for a native handoff"
            );
            let id = input.last().unwrap()["uuid"].as_str().unwrap();
            assert!(
                matches!(
                    history::observed_attempt(
                        "claude",
                        "AgentDesk-claude-i7162-fixture",
                        &[frame()],
                        Some(id)
                    ),
                    history::ObservedAttempt::Pending
                ),
                "{case} must not consume a user receipt even if another observer guard stays quiet"
            );
            assert_eq!(h.placeholder_posts(), 0, "{case}");
            assert!(
                !h.relay_lease_present("AgentDesk-claude-i7162-fixture"),
                "{case}"
            );
            dedupe::clear_tmux_runtime_binding("AgentDesk-claude-i7162-fixture");
        }
    });
}

#[test]
fn legacy_partial_user_is_observed_only_after_newline() {
    if !isolated("legacy_partial_user_is_observed_only_after_newline") {
        return;
    }
    run(async {
        let (h, path) = setup().await;
        register(&path, "busy-attempt", &frame());
        std::fs::write(&path, QUEUED.trim_end_matches('\n')).unwrap();
        assert!(tick(&h).await.is_empty());
        assert_eq!(
            dedupe::runtime_binding_for_tmux_session("AgentDesk-claude-i7162-fixture")
                .unwrap()
                .last_offset,
            0,
            "a partial user must not consume its cursor"
        );
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"\n")
            .unwrap();
        assert_eq!(tick(&h).await.len(), 1);
        dedupe::clear_tmux_runtime_binding("AgentDesk-claude-i7162-fixture");
    });
}

#[test]
fn legacy_two_queued_rows_share_one_response_owner() {
    if !isolated("legacy_two_queued_rows_share_one_response_owner") {
        return;
    }
    run(async {
        let native = NativeFixture::new().await;
        let (h, path) = (&native.h, &native.path);

        let multi = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/tui_input/claude-2.1.316-multi-queued.jsonl"
        ));
        let inputs = rows(multi);
        for (index, input) in inputs
            .iter()
            .filter(|r| r["operation"] == "enqueue")
            .enumerate()
        {
            register(
                &path,
                &format!("multi-{index}"),
                input["content"].as_str().unwrap(),
            );
        }
        std::fs::write(&path, current_rows(&transcript(multi))).unwrap();
        let observed = tick(&h).await;
        assert_eq!(
            observed.len(),
            1,
            "the native batch opens one response turn"
        );
        assert!(
            observed[0].hosted_source.is_some(),
            "batch body requires the native source pin"
        );
        until("one shared batch body", || {
            h.messages().iter().any(|(_, s)| s.contains(BODY))
        })
        .await;
        until("batch tail drained", || {
            !super::super::CLAUDE_IDLE_RESPONSE_TAILS
                .lock()
                .unwrap()
                .contains("AgentDesk-claude-i7162-fixture")
        })
        .await;
        assert_eq!(
            h.messages()
                .iter()
                .filter(|(_, s)| s.contains(BODY))
                .count(),
            1
        );
        assert_eq!(h.placeholder_posts(), 1);
        assert!(tick(&h).await.is_empty());
        dedupe::clear_tmux_runtime_binding("AgentDesk-claude-i7162-fixture");
    });
}

fn incarnation(
    h: &RelayE2eHarness,
    native: &str,
    nonce: &str,
    home: &std::path::Path,
    resumed: bool,
) {
    use crate::services::tmux_common as tc;
    use crate::services::tui_prompt_dedupe::binding_context::{
        BindingContext, PreparedIncarnation,
    };
    let prepared = PreparedIncarnation::create(BindingContext {
        schema: 1,
        provider: "claude".into(),
        created_at: chrono::Utc::now() - chrono::Duration::seconds(1),
        execution_nonce: nonce.into(),
        tmux_session: "AgentDesk-claude-i7162-fixture".into(),
        channel_id: Some(CHANNEL_ID),
        owner_runtime_root: tc::current_tmux_owner_marker(),
        host: None,
        expected_native_session_id: Some(native.into()),
        launch_mode: if resumed { "resume" } else { "fresh" }.into(),
        provider_root: Some(home.into()),
        first_prompt_digest: None,
        source_policy: None,
    })
    .unwrap();
    std::fs::write(
        tc::session_temp_path(
            "AgentDesk-claude-i7162-fixture",
            tc::CLAUDE_TUI_LAUNCH_SCRIPT_TEMP_EXT,
        ),
        format!(
            "{}cd '{}'\nexec 'claude' '{}' '{}'\n",
            prepared.env_lines(),
            h.root.path().display(),
            if resumed { "--resume" } else { "--session-id" },
            native
        ),
    )
    .unwrap();
    std::fs::write(
        tc::session_temp_path("AgentDesk-claude-i7162-fixture", "spawn_nonce"),
        nonce,
    )
    .unwrap();
    tc::host_marker::record_tmux_host_marker("AgentDesk-claude-i7162-fixture");
}

async fn v2_native_case(reincarnated: bool) -> usize {
    use std::os::unix::fs::MetadataExt;
    let (h, _) = setup().await;
    let _events = dedupe::hosted_source::tests::BindingRoot::at(h.root.path());
    let _tmux = crate::services::provider_teardown::tests::test_support::FakeTmux::install(
        "AgentDesk-claude-i7162-fixture",
    );
    let home = h.root.path().join("claude-home");
    let _home = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "CLAUDE_CONFIG_DIR",
        &home,
    );
    let native = "00000000-0000-4000-8000-000000000001";
    let path = crate::services::claude_tui::transcript_tail::claude_transcript_path(
        h.root.path(),
        native,
        None,
    )
    .unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "").unwrap();
    incarnation(&h, native, "11111111111111111111111111111111", &home, false);
    let mut binding =
        dedupe::runtime_binding_for_tmux_session("AgentDesk-claude-i7162-fixture").unwrap();
    binding.output_path = path.display().to_string();
    dedupe::register_tmux_runtime_binding("AgentDesk-claude-i7162-fixture", binding.clone());
    let meta = std::fs::metadata(&path).unwrap();
    let key = json!({"identity":{"primary":1,"queued_generation":1,"sources":[{"message_id":1,"queued_generation":1}]},
        "attempt":{"nonce":"same-nonce","provider":"claude","target":{"host":"tmux","owner":"AgentDesk-claude-i7162-fixture","root":h.root.path(),"execution_nonce":"11111111111111111111111111111111"},
        "source":{"kind":"existing","session_id":native,"transcript":path,"file":[meta.dev(),meta.ino()],"offset":0}}});
    let registry = history::v2::Registry::runtime().unwrap();
    for (namespace, prompt) in [
        ("native-owner", frame()),
        ("other-owner", "other registered frame".into()),
    ] {
        registry.prepare(namespace,&json!({"identity":key["identity"],"attempt":key["attempt"],"original":{"author_id":200},"payload":{"frame":prompt}})).unwrap();
        history::v2::with_attempt(&registry, namespace, &key, || {
            register(&path, "same-nonce", &prompt)
        })
        .unwrap();
    }
    if reincarnated {
        incarnation(&h, native, "22222222222222222222222222222222", &home, true);
        dedupe::register_tmux_runtime_binding("AgentDesk-claude-i7162-fixture", binding);
    }
    let input = rows(QUEUED)
        .into_iter()
        .map(|mut row| {
            row["timestamp"] = json!(chrono::Utc::now().to_rfc3339());
            format!("{row}\n")
        })
        .collect::<String>();
    std::fs::write(&path, &input).unwrap();
    if reincarnated {
        let source =
            crate::services::tui_o::shadow::binding_reader::source_id_for(native, &path).unwrap();
        let proposal = dedupe::binding_events::Proposal {
            channel_id: CHANNEL_ID,
            provider: "claude",
            tmux_session: "AgentDesk-claude-i7162-fixture",
            session_id: Some(native),
            path: path.to_str().unwrap(),
            replaced: None,
            cause: dedupe::binding_events::CauseSource::Observed,
            hook: None,
        };
        dedupe::binding_events::record_verified(&proposal, &source).unwrap();
        assert_eq!(
            dedupe::binding_events::records_strict(CHANNEL_ID)
                .unwrap()
                .unwrap()
                .last()
                .unwrap()
                .execution_nonce
                .as_deref(),
            Some("22222222222222222222222222222222")
        );
    }
    let context_path =
        super::super::launch_script::claude_launch_observation("AgentDesk-claude-i7162-fixture")
            .1
            .unwrap();
    let context = dedupe::hosted_source::scan_context_from_path(
        "claude",
        "AgentDesk-claude-i7162-fixture",
        CHANNEL_ID,
        Some(&context_path),
    )
    .expect("current execution context");
    let user_start = input
        .lines()
        .take(input.lines().count() - 1)
        .map(|line| line.len() as u64 + 1)
        .sum();
    let pin = dedupe::hosted_source::capture_scanned(
        "claude",
        "AgentDesk-claude-i7162-fixture",
        CHANNEL_ID,
        &path,
        Some(&context),
        dedupe::hosted_source::claude_record_created_at(&path, user_start),
    )
    .expect("native reader must get a current execution pin");
    assert_eq!(
        pin.execution_nonce,
        if reincarnated {
            "22222222222222222222222222222222"
        } else {
            "11111111111111111111111111111111"
        }
    );
    let observed = tick(&h).await;
    let saved = registry.json().unwrap();
    let other = saved["live"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["registration"]["namespace"] == "other-owner")
        .unwrap();
    assert!(
        other["events"].as_array().unwrap().is_empty(),
        "same nonce in another namespace must never receive this row"
    );
    dedupe::clear_tmux_runtime_binding("AgentDesk-claude-i7162-fixture");
    observed.len()
}

#[test]
fn legacy_v2_native_reader_keeps_full_namespace_and_attempt_key() {
    if !isolated("legacy_v2_native_reader_keeps_full_namespace_and_attempt_key") {
        return;
    }
    run(async {
        assert_eq!(v2_native_case(false).await, 1);
    });
}
#[test]
fn legacy_v2_native_reader_does_not_borrow_an_old_execution() {
    if !isolated("legacy_v2_native_reader_does_not_borrow_an_old_execution") {
        return;
    }
    run(async {
        assert_eq!(v2_native_case(true).await, 0);
    });
}

#[test]
fn legacy_repeated_frame_keeps_ambiguous_receipt_parked() {
    if !isolated("legacy_repeated_frame_keeps_ambiguous_receipt_parked") {
        return;
    }
    run(async {
        let (h, path) = setup().await;
        register(&path, "old-absorbed", &frame());
        std::fs::write(&path, ABSORBED).unwrap();
        assert_eq!(
            history::receipt("old-absorbed", &path, 0, "enqueue", None, &frame()),
            Some(true)
        );
        let offset = std::fs::metadata(&path).unwrap().len();
        history::register(
            (
                "claude",
                "AgentDesk-claude-i7162-fixture",
                "new-queued",
                "200",
            ),
            &path,
            offset,
            &frame(),
        )
        .unwrap();
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(QUEUED.as_bytes())
            .unwrap();
        assert_eq!(
            history::receipt("new-queued", &path, offset, "enqueue", None, &frame()),
            Some(true)
        );
        assert!(
            tick(&h).await.is_empty(),
            "the existing same-frame CAS must remain conservative"
        );
        assert_eq!(h.placeholder_posts(), 0);
        let id = rows(QUEUED).last().unwrap()["uuid"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(matches!(
            history::observed_attempt(
                "claude",
                "AgentDesk-claude-i7162-fixture",
                &[frame()],
                Some(&id)
            ),
            history::ObservedAttempt::Pending
        ));
        dedupe::clear_tmux_runtime_binding("AgentDesk-claude-i7162-fixture");
    });
}

fn busy_tmux(h: &RelayE2eHarness) -> Vec<crate::config::TestEnvVarGuard> {
    use std::os::unix::fs::PermissionsExt;
    let dir = h.root.path().join("busy-tmux");
    std::fs::create_dir_all(&dir).unwrap();
    let binary = dir.join("tmux");
    std::fs::write(&binary,"#!/bin/sh\nwhile [ \"${1#-}\" != \"$1\" ]; do shift; done\ncase \"$1\" in\nhas-session) exit 0;;\nlist-panes) echo 0; exit 0;;\ncapture-pane) printf '%s\\n' '⏺ Running 1 shell command…' '· Actioning… (4m 7s · esc to interrupt)'; exit 0;;\n*) exit 0;;\nesac\n").unwrap();
    std::fs::set_permissions(binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut paths = vec![dir];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let path = std::env::join_paths(paths).unwrap();
    vec![
        crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "PATH",
            std::path::Path::new(&path),
        ),
        crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_STATUS_INTERVAL_SECS",
            std::path::Path::new("0"),
        ),
    ]
}

fn start_watcher(h: &RelayE2eHarness, path: &std::path::Path, offset: u64) -> AbortOnDrop<()> {
    let watcher = h.shared.tmux_watchers.get(&h.channel_id).unwrap();
    watcher.cancel.store(false, Ordering::Release);
    watcher.last_heartbeat_ts_ms.store(
        crate::services::discord::tmux_watcher_now_ms(),
        Ordering::Release,
    );
    AbortOnDrop(Some(tokio::spawn(Box::pin(
        crate::services::discord::tmux::tmux_output_watcher_with_restore(
            h.channel_id,
            h.ctx.http.clone(),
            h.shared.clone(),
            path.display().to_string(),
            "AgentDesk-claude-i7162-fixture".into(),
            offset,
            watcher.cancel.clone(),
            watcher.paused.clone(),
            watcher.resume_offset.clone(),
            watcher.pause_epoch.clone(),
            watcher.turn_delivered.clone(),
            watcher.last_heartbeat_ts_ms.clone(),
            None,
        ),
    ))))
}

async fn until(label: &str, mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(30), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|error| panic!("{label}: {error}"));
}

fn body_message_ids(h: &RelayE2eHarness) -> std::collections::BTreeSet<u64> {
    h.mock
        .message_writes
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, content)| content.contains(BODY))
        .map(|(id, _)| *id)
        .collect()
}

async fn watcher_case(branch: &str) {
    let native = NativeFixture::new().await;
    let (h, path) = (&native.h, &native.path);
    let _tmux = busy_tmux(&h);
    select_o(false);
    let prior = format!(
        "{}\n{}\n{}\n",
        json!({"type":"user","message":{"role":"user","content":"prior A"}}),
        json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"PRIOR_DELIVERED_A"}]}}),
        json!({"type":"system","subtype":"stop_hook_summary"})
    );
    std::fs::write(&path, &prior).unwrap();
    frontier(&h, prior.len() as u64);
    let mut binding =
        dedupe::runtime_binding_for_tmux_session("AgentDesk-claude-i7162-fixture").unwrap();
    binding.last_offset = prior.len() as u64;
    dedupe::register_tmux_runtime_binding("AgentDesk-claude-i7162-fixture", binding);
    register(&path, "busy-attempt", &frame());
    use std::io::Write;
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(current_rows(QUEUED).as_bytes())
        .unwrap();
    let head = format!(
        "{}\n",
        json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"BUSY_HEAD "}]}})
    );
    let mut observed = Vec::new();
    let watcher;
    if branch == "observer-first" {
        observed = tick(&h).await;
        assert_eq!(
            observed.len(),
            1,
            "observer-first control must reach its observer"
        );
        assert!(observed[0].hosted_source.is_some());
        watcher = start_watcher(&h, &path, prior.len() as u64);
    } else {
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(head.as_bytes())
            .unwrap();
        watcher = start_watcher(&h, &path, prior.len() as u64);
        if branch == "watcher-first" {
            until("watcher row acquired", || {
                inflight::load_inflight_state_read_only(&ProviderKind::Claude, CHANNEL_ID).is_some()
            })
            .await;
        }
        let mut rx = dedupe::subscribe_observed_prompts();
        super::super::claude_idle_runtime::relay_idle_claude_bindings(&h.shared).await;
        observed.extend(
            std::iter::from_fn(|| rx.try_recv().ok())
                .filter(|r| r.tmux_session_name == "AgentDesk-claude-i7162-fixture"),
        );
    }
    let tail = transcript("");
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(tail.as_bytes())
        .unwrap();
    until("watcher/observer B response", || {
        h.messages().iter().any(|(_, s)| s.contains(BODY))
    })
    .await;
    let beat = h
        .shared
        .tmux_watchers
        .get(&h.channel_id)
        .unwrap()
        .last_heartbeat_ts_ms
        .clone();
    let observed_beat = beat.load(Ordering::Acquire);
    until("watcher poll after B POST", || {
        beat.load(Ordering::Acquire) > observed_beat
    })
    .await;
    until("observer tail drained", || {
        !super::super::CLAUDE_IDLE_RESPONSE_TAILS
            .lock()
            .unwrap()
            .contains("AgentDesk-claude-i7162-fixture")
    })
    .await;
    println!(
        "WATCHER_BASELINE branch={branch} ordinary={} body={} placeholder={} note={} posts={} body_message_ids={:?}",
        observed.len(),
        h.messages()
            .iter()
            .filter(|(_, s)| s.contains(BODY))
            .count(),
        h.placeholder_posts(),
        h.messages().iter().filter(|(_, s)| s.contains('⌨')).count(),
        h.mock.channel_posts.lock().unwrap().len(),
        body_message_ids(&h)
    );
    assert_eq!(
        body_message_ids(&h).len(),
        1,
        "body must never appear on two Discord message ids"
    );
    assert_eq!(
        h.messages()
            .iter()
            .filter(|(_, s)| s.contains(BODY))
            .count(),
        1,
        "shared response has one body owner"
    );
    assert!(
        h.placeholder_posts() <= 1,
        "watcher and observer must not post duplicate placeholders"
    );
    if branch == "watcher-first" {
        assert!(observed.is_empty());
    }
    h.shared
        .tmux_watchers
        .get(&h.channel_id)
        .unwrap()
        .cancel
        .store(true, Ordering::Release);
    drop(watcher);
    dedupe::clear_tmux_runtime_binding("AgentDesk-claude-i7162-fixture");
}

#[test]
fn legacy_watcher_first_has_one_body_owner() {
    if !isolated("legacy_watcher_first_has_one_body_owner") {
        return;
    }
    run(watcher_case("watcher-first"));
}
#[test]
fn legacy_observer_first_has_one_body_owner() {
    if !isolated("legacy_observer_first_has_one_body_owner") {
        return;
    }
    run(watcher_case("observer-first"));
}
#[test]
fn legacy_watcher_observer_race_has_one_body_owner() {
    if !isolated("legacy_watcher_observer_race_has_one_body_owner") {
        return;
    }
    run(watcher_case("race"));
}

fn select_o(selected: bool) {
    crate::config_live_reload::install(crate::config::Config {
        tui_o: Some(crate::services::tui_o::shadow::tap::TuiOConfig {
            turn: crate::services::tui_o::turn_mode::TurnConfig {
                channels: Default::default(),
                all_owned: Some(selected),
            },
            ..Default::default()
        }),
        ..Default::default()
    });
}

fn frontier(h: &RelayE2eHarness, end: u64) {
    use crate::services::discord::outbound::delivery_record as dr;
    let marker = crate::services::tmux_common::session_temp_path(
        "AgentDesk-claude-i7162-fixture",
        "generation",
    );
    std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
    std::fs::write(marker, "generation").unwrap();
    dr::write_delivered_frontier(
        &ProviderKind::Claude,
        CHANNEL_ID,
        "AgentDesk-claude-i7162-fixture",
        dr::DeliveredCommit {
            range: (0, end),
            generation_mtime_ns: dr::current_generation_mtime_ns("AgentDesk-claude-i7162-fixture"),
            attempts: 1,
            panel_msg_id: Some(1),
            panel_channel_id: Some(CHANNEL_ID),
        },
    )
    .unwrap();
    assert_eq!(
        dr::resolved_delivered_frontier_end_current_generation(
            &ProviderKind::Claude,
            h.channel_id,
            "AgentDesk-claude-i7162-fixture",
            std::fs::metadata(
                &dedupe::runtime_binding_for_tmux_session("AgentDesk-claude-i7162-fixture")
                    .unwrap()
                    .output_path
            )
            .ok()
            .map(|m| m.len())
        ),
        Some(end)
    );
}

async fn restart_case(delivered: Option<bool>, selected: bool) -> usize {
    let (h, path) = setup().await;
    let _tmux = crate::services::provider_teardown::tests::test_support::FakeTmux::install(
        "AgentDesk-claude-i7162-fixture",
    );
    select_o(selected);
    register(&path, "busy-attempt", &frame());
    let prefix = format!("{}\n", json!({"type":"mode","padding":"a".repeat(70_000)}));
    let all = prefix.clone() + &transcript(QUEUED);
    std::fs::write(&path, &all).unwrap();
    assert_eq!(
        history::receipt(
            "busy-attempt",
            &path,
            prefix.len() as u64,
            "enqueue",
            None,
            &frame()
        ),
        Some(true)
    );
    let mut binding =
        dedupe::runtime_binding_for_tmux_session("AgentDesk-claude-i7162-fixture").unwrap();
    binding.last_offset = all.len() as u64;
    dedupe::register_tmux_runtime_binding("AgentDesk-claude-i7162-fixture", binding);
    if let Some(done) = delivered {
        frontier(&h, if done { all.len() } else { prefix.len() } as u64);
    } else {
        assert_eq!(crate::services::discord::outbound::delivery_record::resolved_delivered_frontier_end_current_generation(&ProviderKind::Claude, h.channel_id, "AgentDesk-claude-i7162-fixture", Some(all.len() as u64)), None);
    }
    assert_eq!(
        crate::services::tui_o::shadow::tap::TuiOConfig::or_default(
            crate::config::runtime_config().tui_o.as_ref()
        )
        .turn
        .selects(CHANNEL_ID),
        selected
    );
    let id = rows(QUEUED).last().unwrap()["uuid"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        matches!(
            history::observed_attempt(
                "claude",
                "AgentDesk-claude-i7162-fixture",
                &[frame()],
                Some(&id)
            ),
            history::ObservedAttempt::Pending
        ),
        "watcher/receipt-thread case has no user receipt before recovery"
    );
    assert!(!crate::services::tui_o::turn_mode::transcript_turns(
        CHANNEL_ID
    ));
    let observed = tick(&h).await;
    if observed.is_empty() {
        assert_eq!(h.placeholder_posts(), 0);
        assert_eq!(h.local_note_posts(), 0);
        assert!(!h.relay_lease_present("AgentDesk-claude-i7162-fixture"));
    }
    dedupe::clear_tmux_runtime_binding("AgentDesk-claude-i7162-fixture");
    observed.len()
}

#[test]
fn legacy_restart_floor_recovers_row_above_frontier_even_when_registration_is_below() {
    if !isolated("legacy_restart_floor_recovers_row_above_frontier_even_when_registration_is_below")
    {
        return;
    }
    run(async {
        assert_eq!(
            restart_case(Some(false), false).await,
            1,
            "unanswered queued row beyond F remains recoverable"
        );
    });
}

#[test]
fn legacy_restart_floor_skips_rows_under_delivered_frontier() {
    if !isolated("legacy_restart_floor_skips_rows_under_delivered_frontier") {
        return;
    }
    run(async {
        assert_eq!(
            restart_case(Some(true), false).await,
            0,
            "a delivered B must open no new owner after restart"
        );
    });
}

#[test]
fn legacy_restart_floor_does_not_treat_unknown_frontier_as_zero() {
    if !isolated("legacy_restart_floor_does_not_treat_unknown_frontier_as_zero") {
        return;
    }
    run(async {
        assert_eq!(restart_case(None, false).await, 0);
    });
}

#[test]
fn legacy_restart_floor_stays_off_for_selected_unconfirmed_o() {
    if !isolated("legacy_restart_floor_stays_off_for_selected_unconfirmed_o") {
        return;
    }
    run(async {
        assert_eq!(restart_case(Some(false), true).await, 0);
    });
}

#[test]
fn legacy_restart_absorbed_registration_cannot_reannounce_typed_row_below_frontier() {
    if !isolated("legacy_restart_absorbed_registration_cannot_reannounce_typed_row_below_frontier")
    {
        return;
    }
    run(async {
        let (h, path) = setup().await;
        let _tmux = crate::services::provider_teardown::tests::test_support::FakeTmux::install(
            "AgentDesk-claude-i7162-fixture",
        );
        select_o(false);
        register(&path, "old-absorbed", &frame());
        let text = ABSORBED.to_owned() + &transcript(TYPED);
        std::fs::write(&path, &text).unwrap();
        frontier(&h, text.len() as u64);
        let mut binding =
            dedupe::runtime_binding_for_tmux_session("AgentDesk-claude-i7162-fixture").unwrap();
        binding.last_offset = text.len() as u64;
        dedupe::register_tmux_runtime_binding("AgentDesk-claude-i7162-fixture", binding);
        assert!(tick(&h).await.is_empty());
        assert_eq!(h.local_note_posts(), 0);
        assert_eq!(h.placeholder_posts(), 0);
        assert!(!h.relay_lease_present("AgentDesk-claude-i7162-fixture"));
        dedupe::clear_tmux_runtime_binding("AgentDesk-claude-i7162-fixture");
    });
}

#[test]
fn legacy_v1_command_receipt_joins_without_new_owner() {
    if !isolated("legacy_v1_command_receipt_joins_without_new_owner") {
        return;
    }
    run(async {
        let (h, path) = setup().await;
        let prompt = human_frame::inject(
            "discord",
            "200",
            "Fixture User",
            "0000ffff",
            "absorbed v1 input",
        );
        register(&path, "0000ffff", &prompt);
        let mut input = rows(ABSORBED);
        for row in &mut input {
            if row.get("content").is_some() {
                row["content"] = json!(prompt);
            }
            if row.pointer("/attachment/prompt").is_some() {
                row["attachment"]["prompt"] = json!(prompt);
            }
        }
        let attachment = input.last().unwrap();
        let row_id = attachment["uuid"]
            .as_str()
            .or_else(|| attachment["attachment"]["source_uuid"].as_str())
            .unwrap();
        std::fs::write(
            &path,
            input
                .iter()
                .map(|row| format!("{row}\n"))
                .collect::<String>(),
        )
        .unwrap();
        let offset = input
            .iter()
            .take(input.len() - 1)
            .map(|row| format!("{row}\n").len() as u64)
            .sum();
        assert_eq!(
            history::receipt("0000ffff", &path, offset, "command", Some(row_id), &prompt),
            Some(true)
        );
        let mut rx = dedupe::subscribe_observed_prompts();
        assert_eq!(
            dedupe::observe_prompt_by_tmux_with_entry_id_at(
                "claude",
                "AgentDesk-claude-i7162-fixture",
                &prompt,
                Some(row_id),
                chrono::Utc::now()
            ),
            dedupe::PromptObservation::InjectedSteer
        );
        assert!(rx.try_recv().is_err());
        assert!(!h.relay_lease_present("AgentDesk-claude-i7162-fixture"));
        assert_eq!(h.placeholder_posts(), 0);
        dedupe::clear_tmux_runtime_binding("AgentDesk-claude-i7162-fixture");
    });
}

async fn v1_context_case(restart: bool) {
    for current in [false, true] {
        let (h, _) = setup().await;
        let _events = dedupe::hosted_source::tests::BindingRoot::at(h.root.path());
        let _tmux = crate::services::provider_teardown::tests::test_support::FakeTmux::install(
            "AgentDesk-claude-i7162-fixture",
        );
        select_o(false);
        let home = h.root.path().join("claude-home");
        let _home = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "CLAUDE_CONFIG_DIR",
            &home,
        );
        let native = "00000000-0000-4000-8000-000000000001";
        let path = crate::services::claude_tui::transcript_tail::claude_transcript_path(
            h.root.path(),
            native,
            None,
        )
        .unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "").unwrap();
        incarnation(&h, native, "11111111111111111111111111111111", &home, false);
        let prefix = format!("{}\n", json!({"type":"mode","padding":"prior source"}));
        let input = rows(QUEUED)
            .into_iter()
            .map(|mut row| {
                if current {
                    row["timestamp"] = json!(chrono::Utc::now().to_rfc3339());
                }
                format!("{row}\n")
            })
            .collect::<String>();
        let text = prefix.clone() + &input;
        register(&path, "busy-attempt", &frame());
        std::fs::write(&path, &text).unwrap();
        let mut binding =
            dedupe::runtime_binding_for_tmux_session("AgentDesk-claude-i7162-fixture").unwrap();
        binding.output_path = path.display().to_string();
        binding.last_offset = if restart {
            text.len() as u64
        } else {
            prefix.len() as u64
        };
        dedupe::register_tmux_runtime_binding("AgentDesk-claude-i7162-fixture", binding);
        if restart {
            frontier(&h, prefix.len() as u64);
        }
        let context_path = super::super::launch_script::claude_launch_observation(
            "AgentDesk-claude-i7162-fixture",
        )
        .1
        .unwrap();
        let context = dedupe::hosted_source::scan_context_from_path(
            "claude",
            "AgentDesk-claude-i7162-fixture",
            CHANNEL_ID,
            Some(&context_path),
        )
        .expect("current context");
        let user_start = prefix.len() as u64
            + input
                .lines()
                .take(input.lines().count() - 1)
                .map(|line| line.len() as u64 + 1)
                .sum::<u64>();
        assert_eq!(
            dedupe::hosted_source::capture_scanned(
                "claude",
                "AgentDesk-claude-i7162-fixture",
                CHANNEL_ID,
                &path,
                Some(&context),
                dedupe::hosted_source::claude_record_created_at(&path, user_start)
            )
            .is_some(),
            current,
            "control must distinguish current native record from old record"
        );
        let observed = tick(&h).await;
        assert_eq!(
            observed.len(),
            usize::from(current),
            "current={current} restart={restart}"
        );
        if !current {
            let id = rows(QUEUED).last().unwrap()["uuid"]
                .as_str()
                .unwrap()
                .to_owned();
            assert!(
                matches!(
                    history::observed_attempt(
                        "claude",
                        "AgentDesk-claude-i7162-fixture",
                        &[frame()],
                        Some(&id)
                    ),
                    history::ObservedAttempt::Pending
                ),
                "old row must not consume the native user receipt"
            );
            assert!(!h.relay_lease_present("AgentDesk-claude-i7162-fixture"));
            assert_eq!(h.placeholder_posts(), 0);
        }
        dedupe::clear_tmux_runtime_binding("AgentDesk-claude-i7162-fixture");
    }
}
#[test]
fn legacy_v1_native_context_rejects_an_old_row_without_consuming_receipt() {
    if !isolated("legacy_v1_native_context_rejects_an_old_row_without_consuming_receipt") {
        return;
    }
    run(v1_context_case(false));
}
#[test]
fn legacy_v1_restart_context_rejects_an_old_row_without_consuming_receipt() {
    if !isolated("legacy_v1_restart_context_rejects_an_old_row_without_consuming_receipt") {
        return;
    }
    run(v1_context_case(true));
}

#[test]
fn legacy_draft_with_a_quoted_frame_remains_human_without_claiming_the_registration() {
    if !isolated("legacy_draft_with_a_quoted_frame_remains_human_without_claiming_the_registration")
    {
        return;
    }
    run(async {
        let (h, path) = setup().await;
        register(&path, "busy-attempt", &frame());
        let mut input = rows(QUEUED);
        let prompt = format!("human draft\n{}", frame());
        input.last_mut().unwrap()["message"]["content"] = json!(prompt);
        std::fs::write(
            &path,
            input.iter().map(|r| format!("{r}\n")).collect::<String>(),
        )
        .unwrap();
        let observed = tick(&h).await;
        assert_eq!(
            observed.len(),
            1,
            "the human draft remains an ordinary human input"
        );
        assert_eq!(observed[0].prompt, prompt);
        let id = input.last().unwrap()["uuid"].as_str().unwrap();
        assert!(matches!(
            history::observed_attempt(
                "claude",
                "AgentDesk-claude-i7162-fixture",
                &[frame()],
                Some(id)
            ),
            history::ObservedAttempt::Pending
        ));
        dedupe::clear_tmux_runtime_binding("AgentDesk-claude-i7162-fixture");
    });
}
#[test]
fn legacy_complete_queued_row_does_not_wait_for_a_torn_later_row() {
    if !isolated("legacy_complete_queued_row_does_not_wait_for_a_torn_later_row") {
        return;
    }
    run(async {
        let (h, path) = setup().await;
        register(&path, "busy-attempt", &frame());
        std::fs::write(&path, format!("{QUEUED}{{\"type\":\"system\",\"subtype\":")).unwrap();
        assert_eq!(tick(&h).await.len(), 1);
        dedupe::clear_tmux_runtime_binding("AgentDesk-claude-i7162-fixture");
    });
}
