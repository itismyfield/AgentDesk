//! Restored input custody is independent of the O writer's source cursor.
use super::super::codex_idle_rollout::{self, PollNote};
use super::super::source_tick::PHASE_PROBE;
use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::tmux_common as tc;
use crate::services::tui_o::ownership::OwnershipGate;
use crate::services::tui_o::shadow::{ShadowProvider, binding_reader::source_id_for};
use crate::services::tui_o::store::{InitSource, Initialized, OStore, StoreConfig};
use crate::services::tui_o::turn_mode::TestConfirmation;
use crate::services::tui_o::writer::host::{self, HostParts, Readiness, test_io::TestHost};
use sha2::{Digest, Sha256};

const CHANNEL: u64 = 7_003_100_001;
const TMUX: &str = "AgentDesk-codex-o-restored-7003";
const SESSION: &str = "019e660d-4859-7522-9cee-8ba7c4e7c743";
const TURN: &str = "019e660d-4859-7522-9cee-8ba7c4e7c744";
const ORIGIN: &str = "[User: probe (ID: 7003)] discord origin";
const PARKED: &str = "한글 direct input parked during handoff";
const ANSWER: &str = "O_RESTORED_PARKED_ANSWER";

fn line(value: serde_json::Value) -> String {
    format!("{value}\n")
}
fn user(text: &str) -> String {
    line(serde_json::json!({"type":"response_item","payload":{
        "type":"message","role":"user","content":[{"type":"input_text","text":text}],
        "internal_chat_message_metadata_passthrough":{"turn_id":TURN,
            "create_time":chrono::Utc::now().timestamp_millis() as f64 / 1000.0,
            "content_item_kinds":["user.text"]}}}))
}
async fn until(label: &str, mut ready: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(20), async {
        while !ready() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|error| panic!("{label} deadline: {error}"));
}
fn notes(note: PollNote) -> usize {
    codex_idle_rollout::POLL_NOTES
        .lock()
        .unwrap()
        .get(&(TMUX.into(), note))
        .copied()
        .unwrap_or(0)
}
fn source_ticks() -> usize {
    PHASE_PROBE.lock().unwrap()[TMUX].0
}

#[test]
fn codex_o_replays_restored_parked_input_without_legacy_body() {
    const CHILD: &str = "ADK_CODEX_O_RESTORED_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let id = concat!(
            "services::discord::tui_prompt_relay::relay_e2e::codex_o_handoff_replay::",
            "codex_o_replays_restored_parked_input_without_legacy_body"
        );
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", id, "--nocapture"])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed; 0 ignored"));
        return;
    }
    let _env_lock = acquire_shared_test_env_lock();
    let (root, _env) = dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
    let _state_lock = dedupe::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    dedupe::reset_state_for_tests();
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        let _tmux = crate::services::provider_teardown::tests::test_support::FakeTmux::install(TMUX);
        let generation = tc::session_temp_path(TMUX, "generation");
        std::fs::create_dir_all(std::path::Path::new(&generation).parent().unwrap()).unwrap();
        std::fs::write(generation, b"1").unwrap();
        let path = root.path().join("restored-rollout.jsonl");
        let header = line(serde_json::json!({"type":"session_meta","payload":{"id":SESSION}}));
        std::fs::write(&path, &header).unwrap();
        let mut shared = crate::services::discord::make_shared_data_for_tests();
        Arc::get_mut(&mut shared).unwrap().provider = ProviderKind::Codex;
        shared.tmux_watchers.restore_owner_channel_for_tmux_session(TMUX, ChannelId::new(CHANNEL));
        dedupe::register_tmux_channel(TMUX, CHANNEL);
        dedupe::register_tmux_runtime_binding(TMUX, dedupe::TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::CodexTui,
            output_path: path.display().to_string(), relay_output_path: None,
            input_fifo_path: None, session_id: Some(SESSION.into()),
            last_offset: header.len() as u64, relay_last_offset: None,
        });
        crate::services::codex_tui::session::write_codex_tui_rollout_marker(TMUX, &path, Some(SESSION)).unwrap();
        // Boot confirms O before either the writer or the existing relay starts.
        let _confirmed = TestConfirmation::new(CHANNEL);
        let _selected = crate::services::tui_o::cutover::test_override::force_candidates(&[(CHANNEL, RuntimeHandoffKind::CodexTui)]);
        let handoff = dedupe::reserve_discord_handoff("codex", TMUX, ORIGIN);
        dedupe::record_discord_originated_prompt("codex", TMUX, ORIGIN);
        assert_eq!(dedupe::observe_codex_prompt_in_turn_at(TMUX, PARKED, None, Some(TURN), chrono::Utc::now()),
            dedupe::PromptObservation::ParkedForHandoff);
        assert_eq!(dedupe::held_handoff_inputs_for_tests("codex", TMUX), Some(1));
        assert!(dedupe::durable_handoff_present_for_tests("codex", TMUX));
        std::mem::forget(handoff);
        let records = line(serde_json::json!({"type":"event_msg","payload":{"type":"task_started","turn_id":TURN}}))
            + &user(ORIGIN) + &user(PARKED)
            + &line(serde_json::json!({"type":"response_item","payload":{"id":"o-restored-answer","type":"message",
                "role":"assistant","content":[{"type":"output_text","text":ANSWER}]}}))
            + &line(serde_json::json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":TURN,"last_agent_message":ANSWER}}));
        { use std::io::Write; std::fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(records.as_bytes()).unwrap(); }
        let eof = std::fs::metadata(&path).unwrap().len();
        dedupe::reset_state_for_tests();
        dedupe::register_tmux_channel(TMUX, CHANNEL);
        let binding = super::super::codex_tui_rehydrated_binding_from_rollout_path(TMUX, &path, Some(SESSION.into())).unwrap();
        assert!(binding.last_offset < eof, "custody has its own replay floor");
        dedupe::register_tmux_runtime_binding(TMUX, binding.clone());
        assert!(dedupe::codex_restore_has_custody(TMUX).unwrap());
        let shared_offset = binding.last_offset;
        let observed_before = dedupe::codex_restore_observation_for_tests(TMUX).unwrap();
        let mut observed = dedupe::subscribe_observed_prompts();
        PHASE_PROBE.lock().unwrap().insert(TMUX.into(), (0, 0, false));
        let source = source_id_for(SESSION, &path).unwrap();
        let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, root.path()).unwrap().unwrap();
        let era = store.begin_era(&[CHANNEL], chrono::Utc::now(), |channel| Ok(Initialized {
            channel, sources: vec![InitSource { source_id: source.clone(),
                delivery_start: header.len() as u64, prefix_hash: hex::encode(Sha256::digest(header.as_bytes())) }],
            initial_anchor: 100, build_digest: "test".into(), at: chrono::Utc::now(),
        })).unwrap();
        let io = TestHost::new([(CHANNEL, source.clone())]);
        io.sessions.lock().unwrap().insert(CHANNEL, TMUX.into());
        let gate = Arc::new(OwnershipGate::default()); gate.acquired();
        let ready = Arc::new(Readiness::default());
        let tasks = host::start(ShadowProvider::Codex, true, || HostParts {
            io: io.clone(), runtime_root: Some(root.path().into()), gate: gate.clone(), readiness: ready.clone(),
        });
        until("O writer final body", || io.posts.to(CHANNEL).iter().filter(|body| body.as_str()==ANSWER).count()==1).await;
        until("O source consumed", || store.open_channel(&era, CHANNEL).unwrap().unwrap()
            .cursor(&source).is_some_and(|cursor| cursor.captured_through==eof)).await;
        assert!(dedupe::codex_restore_has_custody(TMUX).unwrap(), "spooling cannot release input custody");
        assert_eq!(dedupe::runtime_binding_for_tmux_session(TMUX).unwrap().last_offset, shared_offset);
        assert!(observed.try_recv().is_err(), "writer never replays parked observations");
        super::super::spawn_tui_prompt_relay(shared.clone(), ProviderKind::Codex);
        until("idle worker releases restored custody", || !dedupe::codex_restore_has_custody(TMUX).unwrap()
            && dedupe::codex_restore_settled_for_tests(TMUX)==Some(1)).await;
        assert_eq!(dedupe::held_handoff_inputs_for_tests("codex", TMUX), Some(0));
        let events: Vec<_> = std::iter::from_fn(|| observed.try_recv().ok()).collect();
        let parked: Vec<_> = events.iter().filter(|e| e.tmux_session_name==TMUX && e.prompt==PARKED).collect();
        assert_eq!(parked.len(), 1, "parked input re-observed exactly once");
        assert!(parked[0].steer_echo);
        assert_eq!(parked[0].native_turn_id.as_deref(), Some(TURN));
        assert!(matches!(dedupe::observe_codex_prompt_in_turn_at(TMUX, "next fresh direct input", None,
            Some("019e660d-4859-7522-9cee-8ba7c4e7c745"), chrono::Utc::now()),
            dedupe::PromptObservation::PublishedSshDirect | dedupe::PromptObservation::JoinedNativeTurn));
        let before = source_ticks(); until("additional idle ticks", || source_ticks()>=before+3).await;
        assert_eq!(PHASE_PROBE.lock().unwrap()[TMUX].1, 0, "Legacy body never entered");
        assert_eq!(notes(PollNote::BodyScan), 0);
        assert_eq!(notes(PollNote::SharedOffset), 0);
        assert!(!codex_idle_rollout::TAIL_STARTS.lock().unwrap().contains(&CHANNEL));
        assert_eq!(dedupe::runtime_binding_for_tmux_session(TMUX).unwrap().last_offset, shared_offset);
        assert_eq!(dedupe::codex_restore_observation_for_tests(TMUX).unwrap().3, observed_before.3,
            "Legacy unknown-span accounting did not run");
        assert_eq!(io.posts.to(CHANNEL).iter().filter(|body| body.as_str()==ANSWER).count(), 1);
        assert!(!std::iter::from_fn(|| observed.try_recv().ok()).any(|e| e.prompt==PARKED));
        for task in tasks { task.abort(); let _ = task.await; }
    });
    dedupe::reset_state_for_tests();
}
