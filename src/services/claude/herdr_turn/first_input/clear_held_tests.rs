use super::tests::NONCE;
use super::*;
use crate::services::claude_tui::{hook_server::HookEventKind, readiness};
use crate::services::discord::{
    owner_drive_for_test as drive, owner_effect_for_test as effect,
    owner_held_watch_for_test as held_watch,
};
use crate::services::session_host::herdr_socket_rig_tests::{HerdrRig, KEY, NODE, PANE, SESSION};
use crate::services::tui_prompt_dedupe::{
    self as dedupe, binding_context, binding_events as events,
};
use serde_json::{Value, json};
use std::os::unix::fs::MetadataExt;

struct Source {
    context: binding_context::BindingContext,
    path: PathBuf,
    previous_root: Option<PathBuf>,
    channel: u64,
}
impl Source {
    fn new(root: &Path, logical: &str, channel: u64) -> Self {
        readiness::initialize();
        let project = root.join("projects/project");
        std::fs::create_dir_all(&project).unwrap();
        let native = uuid::Uuid::new_v4().to_string();
        let old = project.join(format!("{native}.jsonl"));
        std::fs::write(
            &old,
            "{\"type\":\"system\",\"subtype\":\"turn_duration\"}\n",
        )
        .unwrap();
        let context: binding_context::BindingContext = serde_json::from_value(json!({
            "schema":1,"provider":"claude","created_at":chrono::Utc::now(),
            "execution_nonce":NONCE,"tmux_session":logical,"channel_id":channel,
            "owner_runtime_root":"test","host":"herdr","expected_native_session_id":native,
            "launch_mode":"fresh","provider_root":root.join("projects")
        }))
        .unwrap();
        binding_context::PreparedIncarnation::create(context.clone()).unwrap();
        let marker = crate::services::tmux_common::session_temp_path(logical, "spawn_nonce");
        std::fs::create_dir_all(Path::new(&marker).parent().unwrap()).unwrap();
        std::fs::write(marker, NONCE).unwrap();
        dedupe::register_tmux_runtime_binding(
            logical,
            TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::ClaudeTui,
                output_path: old.display().to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: Some(native.clone()),
                last_offset: 0,
                relay_last_offset: None,
            },
        );
        let previous_root = events::test_root();
        events::set_test_root(Some(root));
        let payload = json!({"source":"startup","session_id":native,"transcript_path":old});
        let hook = events::HookSignal::from_payload("session_start", &payload);
        let meta = std::fs::metadata(&old).unwrap();
        events::record_verified(
            &events::Proposal {
                channel_id: channel,
                provider: "claude",
                tmux_session: logical,
                session_id: Some(&native),
                path: old.to_str().unwrap(),
                replaced: None,
                cause: events::CauseSource::Hook(events::BindingCause::Startup),
                hook: Some(&hook),
            },
            &events::SourceId {
                session_id: native.clone(),
                path: old.clone(),
                dev: meta.dev(),
                ino: meta.ino(),
            },
        )
        .unwrap();
        let mut source = Self {
            context,
            path: old,
            previous_root,
            channel,
        };
        source.clear();
        assert!(
            readiness::observe(logical, Some(&source.path)).is_some(),
            "fixture needs an authenticated pending clear"
        );
        source
    }
    fn clear(&mut self) {
        let native = uuid::Uuid::new_v4().to_string();
        self.path = self.path.with_file_name(format!("{native}.jsonl"));
        let payload = json!({"source":"clear","session_id":native,"transcript_path":self.path});
        let hook = events::HookSignal::from_payload("session_start", &payload);
        events::record_pending(&events::Proposal {
            channel_id: self.channel,
            provider: "claude",
            tmux_session: &self.context.tmux_session,
            session_id: Some(&native),
            path: self.path.to_str().unwrap(),
            replaced: None,
            cause: events::CauseSource::Hook(events::BindingCause::Clear),
            hook: Some(&hook),
        })
        .unwrap();
        readiness::note_hook(
            "claude",
            &HookEventKind::SessionStart,
            &self.context.tmux_session,
            &payload,
            Some(&self.context),
            Some(chrono::Utc::now()),
            false,
        );
    }
    fn attached(&self, target: HerdrTarget) -> Attached {
        Attached {
            nonce: NONCE.into(),
            session_id: self.path.file_stem().unwrap().to_str().unwrap().into(),
            transcript: self.path.clone(),
            target,
            bound: true,
            awaiting_clear: false,
            physical: None,
        }
    }
}
impl Drop for Source {
    fn drop(&mut self) {
        dedupe::clear_tmux_runtime_binding(&self.context.tmux_session);
        events::set_test_root(self.previous_root.as_deref());
    }
}

#[derive(Clone, Copy)]
enum Case {
    Draft,
    Capture,
    ChangedPath,
    ChangedNonce,
    CommonDraft,
    CommonCurrent,
}
fn capture(draft: &str) -> Value {
    json!({"type":"pane_read","read":{"pane_id":PANE,"source":"recent_unwrapped",
        "text":format!("✻ Baked for 2s\n\n{}\n❯ {draft}\n{}\n  🤖 Opus(H) │ 7% │ MCP: 2\n  ⏵⏵ bypass permissions on (shift+tab to cycle)","─".repeat(52),"─".repeat(52)),"revision":1,"truncated":false}})
}
fn run(case: Case) {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let rig = HerdrRig::start();
    let _registry = rig.registry_on_this_thread();
    let install = rig.thread_installer(NODE, KEY, rig.socket(), SESSION);
    let events_root = root.path().to_path_buf();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .on_thread_start(move || {
            install();
            events::set_test_root(Some(&events_root));
        })
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    let logical = "herdr-clear-held";
    let marker = crate::services::tmux_common::session_temp_path(logical, "host_kind");
    std::fs::create_dir_all(Path::new(&marker).parent().unwrap()).unwrap();
    std::fs::write(marker, "herdr").unwrap();
    let channel = 7_274_880 + case as u64;
    let mut source = Source::new(root.path(), logical, channel);
    rig.run_provider(
        &binding_context::context_path("claude", NONCE).unwrap(),
        false,
    );
    let draft_pane = crate::services::claude_tui::host_input::FakeDraftPane::new(logical);
    draft_pane.show(capture("human draft")["read"]["text"].as_str().unwrap());
    let old_binding = dedupe::runtime_binding_for_tmux_session(logical)
        .unwrap()
        .output_path;
    let (_, target) = super::tests::bound_target(&rig, logical, channel);
    let attached = source.attached(target.clone());
    let registry = crate::services::turn_orchestrator::ChannelMailboxRegistry::default();
    let mailbox = registry.handle(poise::serenity_prelude::ChannelId::new(channel));
    let attempt = json!({"nonce":"clear-held-1","provider":"claude",
        "target":{"host":"herdr","owner":logical,"root":rig.socket().display().to_string(),"execution_nonce":NONCE},
        "source":{"kind":"pre_source","expected_session_id":attached.session_id,
            "expected_transcript":source.path,"binding_watermark":null}});
    runtime
        .block_on(mailbox.v2_cold_fixture(
            root.path().join("held-clear.json"),
            attempt.clone(),
            "one original".into(),
            channel,
        ))
        .unwrap();
    rig.answer(
        "pane.read",
        if matches!(case, Case::Capture) {
            json!({"unavailable":true})
        } else {
            capture("human draft")
        },
    );
    let writer = format!("{}#{PANE}", rig.socket().display());
    let watched_path = source.path.clone();
    let leaf = effect::created(
        Box::new(move |frame| {
            guarded_outcome::capture_preparation(|| {
                let (sender, _receiver) = std::sync::mpsc::channel();
                assert!(
                    readiness::observe(logical, Some(&watched_path)).is_some(),
                    "owned caller must see its authenticated clear proof before the veto"
                );
                if !matches!(case, Case::Capture) {
                    let pane = attached
                        .target
                        .capture_ansi(-80)
                        .expect("owned draft capture exists");
                    assert!(!readiness::empty_draft(&pane));
                }
                let result = write(logical, None, attached, &frame, &sender).map(drop);
                match case {
                    Case::CommonDraft => readiness::watch_draft(logical, Some(&watched_path)),
                    Case::CommonCurrent => readiness::watch_current(logical, Some(&watched_path)),
                    _ => (),
                }
                result
            })
        }),
        Some(writer.clone()),
    );
    let held = runtime
        .block_on(effect::run(&mailbox, Arc::new(|| 10000), leaf))
        .unwrap();
    assert!(matches!(held, effect::Completion::NotSent(_)), "{held:?}");
    let snapshot = runtime
        .block_on(mailbox.v2_request(|s| s.as_ref().unwrap().json()))
        .unwrap();
    assert!(snapshot["head_injection"].is_null());
    assert_eq!(snapshot["items"].as_array().unwrap().len(), 1);
    assert_eq!(snapshot["items"][0]["text"], "one original");
    assert!(snapshot["retained_inputs"].as_array().unwrap().is_empty());
    assert!(rig.sends().is_empty());
    let message = snapshot["items"][0]["message_id"].as_u64().unwrap();
    let attached = source.attached(target);
    rig.answer("pane.read", capture(""));
    draft_pane.show(capture("")["read"]["text"].as_str().unwrap());
    match case {
        Case::ChangedPath => {
            source.clear();
            assert!(readiness::observe(logical, Some(&source.path)).is_some());
        }
        Case::ChangedNonce => {
            let marker = crate::services::tmux_common::session_temp_path(logical, "spawn_nonce");
            let mut replacement = source.context.clone();
            replacement.execution_nonce = uuid::Uuid::new_v4().simple().to_string();
            binding_context::PreparedIncarnation::create(replacement.clone()).unwrap();
            std::fs::write(marker, &replacement.execution_nonce).unwrap();
            let native = source.path.file_stem().unwrap().to_str().unwrap();
            readiness::note_hook(
                "claude",
                &HookEventKind::SessionStart,
                logical,
                &json!({"source":"clear","session_id":native,"transcript_path":source.path}),
                Some(&replacement),
                Some(chrono::Utc::now()),
                false,
            );
            assert!(readiness::observe(logical, Some(&source.path)).is_none());
        }
        _ => (),
    }
    let (completed, mut completion) = tokio::sync::oneshot::channel();
    let completed = Arc::new(std::sync::Mutex::new(Some(completed)));
    let attached = Arc::new(std::sync::Mutex::new(Some(attached)));
    let (m, a) = (mailbox.clone(), attempt.clone());
    let drive = Arc::new(drive::Drive {
        mailbox: mailbox.clone(),
        channel,
        handoff: Arc::new(move |front| {
            assert_eq!(front.1.message_id, message);
            assert_eq!(front.1.text, "one original");
            let (m, mut a, attached, done, writer) = (
                m.clone(),
                a.clone(),
                attached.lock().unwrap().take().unwrap(),
                completed.clone(),
                writer.clone(),
            );
            Box::pin(async move {
                a["nonce"] = "clear-held-2".into();
                m.v2_request(move |slot| {
                    slot.as_mut()
                        .unwrap()
                        .reserve(
                            serde_json::from_value(a).unwrap(),
                            serde_json::from_value(
                                json!({"frame":"one original","digest":"fixture","attribution":{}}),
                            )
                            .unwrap(),
                        )
                        .map(drop)
                })
                .await?;
                let leaf = effect::created(
                    Box::new(move |frame| {
                        guarded_outcome::capture_preparation(|| {
                            let (sender, _receiver) = std::sync::mpsc::channel();
                            write(logical, None, attached, &frame, &sender).map(drop)
                        })
                    }),
                    Some(writer),
                );
                let result = effect::run(&m, Arc::new(|| 10001), leaf).await?;
                done.lock().unwrap().take().unwrap().send(result).unwrap();
                Ok(())
            })
        }),
        settled: None,
    });
    let observed = runtime.block_on(async {
        let (_, read) = held_watch::watch_observed(drive, message);
        tokio::time::timeout(Duration::from_secs(5), read)
            .await
            .unwrap()
            .unwrap()
    });
    if matches!(case, Case::ChangedPath | Case::ChangedNonce) {
        assert_eq!(observed.verdict, held_watch::Verdict::Keep);
        assert!(matches!(
            completion.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        assert!(rig.sends().is_empty());
        return;
    }
    assert_eq!(
        observed.verdict,
        held_watch::Verdict::Wake,
        "clear held watch must reuse its explicit source"
    );
    let result = runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(5), completion)
            .await
            .unwrap()
            .unwrap()
    });
    assert_eq!(result, effect::Completion::Submitted);
    let sends = rig.sends();
    assert_eq!(
        sends.len(),
        2,
        "same original needs one paste and one Enter: {sends:?}"
    );
    assert_eq!(
        sends
            .iter()
            .filter(|s| s["text"].as_str() == Some("\u{1b}[200~one original\u{1b}[201~"))
            .count(),
        1
    );
    assert_eq!(
        sends
            .iter()
            .filter(|s| s["keys"]
                .as_array()
                .is_some_and(|k| k.iter().any(|v| v == "enter")))
            .count(),
        1
    );
    let snapshot = runtime
        .block_on(mailbox.v2_request(|s| s.as_ref().unwrap().json()))
        .unwrap();
    assert!(snapshot["items"].as_array().unwrap().is_empty());
    assert!(snapshot["retained_inputs"].as_array().unwrap().is_empty());
    assert!(
        !source.path.exists(),
        "first submission precedes JSONL publication"
    );
    assert_eq!(
        dedupe::runtime_binding_for_tmux_session(logical)
            .unwrap()
            .output_path,
        old_binding
    );
}

#[test]
fn clear_draft_held_owner_auto_wakes_and_submits_the_original_once() {
    run(Case::Draft)
}
#[test]
fn clear_capture_held_owner_auto_wakes_and_submits_the_original_once() {
    run(Case::Capture)
}
#[test]
fn clear_held_watch_never_wakes_for_a_different_latest_path() {
    run(Case::ChangedPath)
}
#[test]
fn clear_held_watch_never_wakes_for_a_different_nonce() {
    run(Case::ChangedNonce)
}
#[test]
fn clear_shared_draft_watch_keeps_the_explicit_source() {
    run(Case::CommonDraft)
}
#[test]
fn clear_shared_current_watch_keeps_the_explicit_source() {
    run(Case::CommonCurrent)
}
