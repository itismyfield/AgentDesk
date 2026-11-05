//! The live gateway admission must not park safe turn-mode input on a legacy busy veto.
use super::*;
use crate::services::discord::turn_presence::{
    activity::tests::{BindingRoot, ReadyPane, bind_turn_mode_transcript},
    supervisor::{self, tests::LiveSupervisor},
};
use std::io::Write;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bb_live_rollover_uses_real_intake_and_redelivery_is_not_reinjected_pg() {
    let busy = busy().await;
    let rt = &busy.rt;
    busy.pane.set(InjectPane::TRANSCRIPT, &format!("{}\n{}\n",
        serde_json::json!({"type":"user","uuid":"first-native","message":{"content":"go"}}),
        serde_json::json!({"type":"assistant","uuid":"first-answer","apiBlockIndex":0,"message":{"id":"first-answer","content":[{"type":"text","text":"working"}]}})));
    rt.hold_mailbox().await;
    let events = tempfile::tempdir().unwrap();
    let _events = BindingRoot::enter(events.path());
    let session = busy.pane.session();
    let _ready = ReadyPane::mark(&session);
    let _confirmed = bind_turn_mode_transcript(
        &rt.h.shared,
        &ProviderKind::Claude,
        CHANNEL_ID,
        &session,
        &busy.pane.path(InjectPane::TRANSCRIPT),
    );
    let presence = Arc::new(LiveSupervisor::new(
        rt.h.shared.clone(),
        ProviderKind::Claude,
        CHANNEL_ID,
    ));
    assert!(supervisor::observes(&ProviderKind::Claude, CHANNEL_ID));
    let first = fresh_id();
    rt.h.deliver_user_message(first, "first input")
        .await
        .unwrap();
    assert_eq!(busy.pane.keys(), ["paste-buffer", "send-keys"]);
    assert_eq!(rt.marks(first), ["➕"]);
    let frame = busy.pane.read("buffer").unwrap();
    let mut transcript = std::fs::OpenOptions::new()
        .append(true)
        .open(busy.pane.path(InjectPane::TRANSCRIPT))
        .unwrap();
    for row in [
        serde_json::json!({"type":"system","subtype":"turn_duration"}),
        serde_json::json!({"type":"user","uuid":"next-native","message":{"content":frame}}),
        serde_json::json!({"type":"assistant","uuid":"next-answer","apiBlockIndex":0,"message":{"id":"next-answer","content":[{"type":"text","text":"working"}]}}),
        serde_json::json!({"type":"permission-mode","mode":"bypassPermissions"}),
    ] {
        writeln!(transcript, "{row}").unwrap();
    }
    std::fs::remove_file(busy.pane.path("pasted")).unwrap();
    assert!(
        !crate::services::tui_turn_state::observe_claude_jsonl_turn_state(
            &busy.pane.path(InjectPane::TRANSCRIPT)
        )
        .is_busy()
    );
    let second = fresh_id();
    rt.h.deliver_user_message(second, "next Discord input")
        .await
        .unwrap();
    let observed = (
        hook::seen(second).outcomes,
        rt.queue().await,
        busy.pane.keys(),
        rt.marks(second),
        rt.h.checkpoint(),
        rt.starts(),
        on_disk(second),
    );
    rt.h.deliver_user_message(second, "next Discord input")
        .await
        .unwrap();
    let keys_after_redelivery = busy.pane.keys();
    rt.finish().await;
    let taken = (
        vec!["Injected { turn_id: None }".to_string()],
        vec![],
        vec![
            "paste-buffer".to_string(),
            "send-keys".to_string(),
            "paste-buffer".to_string(),
            "send-keys".to_string(),
        ],
        vec!["➕".to_string()],
        Some(second),
        0,
        Some("\"observed\"".to_string()),
    );
    assert_eq!(
        observed, taken,
        "production live intake must not fall back to its queue"
    );
    assert_eq!(
        keys_after_redelivery, observed.2,
        "the source terminal record prevents a second paste"
    );
    assert_eq!(
        rt.h.shared
            .turn_view_reconciler
            .pending_injected_count(ChannelId::new(CHANNEL_ID)),
        2
    );
    let before_idle = rt.h.shared.turn_view_reconciler.ops().len();
    assert!(
        wait_until(WAIT, || {
            let presence = presence.clone();
            Box::pin(async move {
                presence.tick().await;
                presence.state()["activity"] == "busy"
            })
        })
        .await,
        "the real transcript reader must publish Busy before closing the turn: {}",
        presence.state()
    );
    writeln!(
        transcript,
        "{}",
        serde_json::json!({"type":"system","subtype":"turn_duration"})
    )
    .unwrap();
    busy.pane.idle();
    assert!(
        wait_until(WAIT, || {
            let presence = presence.clone();
            let shared = rt.h.shared.clone();
            Box::pin(async move {
                presence.tick().await;
                presence.state()["activity"] == "idle"
                    && [first, second].iter().all(|message| {
                        shared.turn_view_reconciler.ops().iter().any(|op| {
                            op.target.message_id.get() == *message && op.emoji == '✅' && op.add
                        })
                    })
            })
        })
        .await,
        "registered turn-mode intake must complete from the real transcript Idle: {}",
        presence.state()
    );
    for message in [first, second] {
        let changes: Vec<_> =
            rt.h.shared
                .turn_view_reconciler
                .ops()
                .into_iter()
                .skip(before_idle)
                .filter(|op| op.target.message_id.get() == message)
                .map(|op| (op.emoji, op.add))
                .collect();
        assert_eq!(changes, [('✅', true)]);
    }
    assert!(
        rt.h.shared.turn_view_reconciler.ops().iter().all(|op| {
            ![first, second].contains(&op.target.message_id.get()) || op.emoji != '➕' || op.add
        }),
        "idle completion must preserve both absorption receipts"
    );
    assert_eq!(
        rt.h.shared
            .turn_view_reconciler
            .pending_injected_count(ChannelId::new(CHANNEL_ID)),
        0
    );
}

async fn owner_fixture() -> Busy {
    let rt = runtime().await;
    let channel = ChannelId::new(CHANNEL_ID);
    assert!(
        rt.h.shared
            .tmux_watchers
            .channel_binding(&channel)
            .is_none()
    );
    assert!(
        crate::services::discord::inflight::load_inflight_state_read_only(
            &ProviderKind::Claude,
            CHANNEL_ID,
        )
        .is_none()
    );
    // Observe Unbound before reusing the fixed fixture channel's reader key.
    let initial = crate::services::discord::turn_presence::activity::activity_now(
        &rt.h.shared,
        &ProviderKind::Claude,
        channel,
    )
    .await;
    assert_ne!(
        initial.activity,
        crate::services::discord::turn_presence::activity::Activity::Busy,
        "an unbound bootstrap must invalidate the previous fixture reader: {initial:?}"
    );
    Busy {
        pane: InjectPane::new(CHANNEL_ID, "off"),
        _gate: hook::open_gate(CHANNEL_ID),
        rt,
    }
}

struct OwnerWakePause {
    channel: ChannelId,
    previous: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl OwnerWakePause {
    fn new(channel: ChannelId) -> Self {
        let previous = crate::services::discord::queue_io::cancel_backstop_test_support::set_owner_wake_for_tests(
            channel,
            Some(Arc::new(|| {})),
        );
        Self { channel, previous }
    }
}

impl Drop for OwnerWakePause {
    fn drop(&mut self) {
        crate::services::discord::queue_io::cancel_backstop_test_support::set_owner_wake_for_tests(
            self.channel,
            self.previous.take(),
        );
    }
}

/// The scripted pane also answers the owner's liveness and physical identity reads.
fn owner_tmux(
    pane: &InjectPane,
    _env: &crate::config_live_reload::TestLiveGuard,
) -> crate::config::TestEnvVarGuard {
    assert!(
        crate::config::shared_test_env_lock().try_lock().is_err(),
        "the borrowed harness must own the shared env lock before PATH mutation"
    );
    let path = pane.path("tmux");
    let script = std::fs::read_to_string(&path).unwrap();
    let owner_reads = r##"for owner_format do :; done
case "$2" in
has-session) exit 0 ;;
list-panes) printf '0\n'; exit 0 ;;
-V) printf 'tmux 3.4\n'; exit 0 ;;
if-shell)
  case "$7" in *PINNED_KEY_APPLIED*)
    if [ -f "$d/pinned_held" ]; then printf 'PINNED_KEY_HELD\n'; exit 0; fi
    case "$6" in
      *'#{==:#{pid},101}'*'#{==:#{session_id},$1}'*'#{==:#{window_id},@1}'*'#{==:#{pane_id},%1}'*'#{==:#{pane_pid},102}'*) ;;
      *) printf 'PINNED_KEY_HELD\n'; exit 0 ;;
    esac ;;
  esac ;;
display-message)
  if [ "$owner_format" = '#{pane_in_mode}' ]; then printf '0\n'; exit 0; fi
  if [ "$owner_format" = '#{pid} #{session_id} #{window_id} #{pane_id} #{pane_pid}' ]; then
    printf '101 $1 @1 %%1 102\n'; exit 0
  fi ;;
esac
"##;
    let script = script.replacen(
        "case \"$2\" in\n",
        &format!("{owner_reads}case \"$2\" in\n"),
        1,
    );
    let pinned_ack = r##"case "$2" in
if-shell) case "$7" in *PINNED_KEY_APPLIED*) printf 'PINNED_KEY_APPLIED\n' ;; esac ;;
esac
"##;
    let script = script.replacen(
        "\nesac\nexit 0\n",
        &format!("\nesac\n{pinned_ack}exit 0\n"),
        1,
    );
    std::fs::write(&path, script).unwrap();
    let session = pane.session();
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let context = serde_json::json!({
        "schema": 1,
        "provider": "claude",
        "created_at": chrono::Utc::now(),
        "execution_nonce": nonce,
        "tmux_session": session,
        "channel_id": CHANNEL_ID,
        "owner_runtime_root": crate::services::tmux_common::current_tmux_owner_marker(),
        "host": "reaction-owner-test",
        "expected_native_session_id": null,
        "launch_mode": "fresh",
        "provider_root": null,
    });
    crate::services::tui_prompt_dedupe::binding_context::PreparedIncarnation::create(
        serde_json::from_value(context).unwrap(),
    )
    .unwrap();
    let marker = std::path::PathBuf::from(crate::services::tmux_common::session_temp_path(
        &session,
        "spawn_nonce",
    ));
    std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
    std::fs::write(marker, nonce).unwrap();
    crate::config::TestEnvVarGuard::prepend_path_after_shared_test_env_lock(path.parent().unwrap())
}

async fn owner_install(rt: &Runtime, pane: &InjectPane, active: bool) {
    use crate::services::discord::queue_io::input_ingress as ingress;
    let channel = ChannelId::new(CHANNEL_ID);
    let generation = crate::services::discord::runtime_store::generation_path().unwrap();
    std::fs::create_dir_all(generation.parent().unwrap()).unwrap();
    std::fs::write(generation, "7").unwrap();
    assert_ne!(
        crate::services::discord::runtime_store::process_generation(),
        0,
        "the owner must receive a settled source generation"
    );
    crate::services::turn_orchestrator::test_support::queue_save_faults(channel, true);
    let name = format!("inject-{CHANNEL_ID}");
    rt.h.mock.name_channel(CHANNEL_ID, &name);
    let mut binding =
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(&pane.session())
            .expect("the scripted pane has its live transcript binding");
    binding.session_id = Some(InjectPane::SESSION.into());
    crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(&pane.session(), binding);
    {
        let mut core = rt.h.shared.core.lock().await;
        let session = core
            .sessions
            .get_mut(&channel)
            .expect("bound fixture session");
        session.channel_name = Some(name);
        session.session_id = Some(InjectPane::SESSION.into());
    }
    assert_eq!(
        crate::services::platform::tmux::pane_liveness(&pane.session()),
        crate::services::platform::tmux::PaneLiveness::Live,
        "the owner's registration must observe the scripted live pane"
    );
    if active {
        rt.hold_mailbox().await;
    }
    assert_eq!(
        rt.h.shared
            .mailbox(channel)
            .has_active_turn()
            .await
            .unwrap(),
        active
    );
    assert!(
        ingress::ADOPTS
            .scope(
                true,
                ingress::adopt(&rt.h.shared, &ProviderKind::Claude, channel)
            )
            .await
            .unwrap(),
        "the real local-TUI admission must install the owner before intake"
    );
    assert!(
        ingress::installed(&rt.h.shared.mailbox(channel))
            .await
            .unwrap()
    );
}

/// ADOPTS belongs to the spawned handler task, just as the event itself does.
async fn owner_deliver(rt: &Runtime, message: u64, text: &str) {
    let event = serenity::FullEvent::Message {
        new_message: user_message(message, text),
    };
    let (ctx, data) = (rt.h.ctx.clone(), rt.h.clone_data());
    tokio::time::timeout(
        WAIT,
        tokio::spawn(async move {
            crate::services::discord::queue_io::input_ingress::ADOPTS
                .scope(true, super::super::boxed_intake(&ctx, &event, &data))
                .await
        }),
    )
    .await
    .expect("the actual owner intake task finishes")
    .expect("owner intake task")
    .expect("owner accepts the original");
    assert_eq!(
        hook::seen(message).offers,
        0,
        "an installed owner bypasses the legacy offer"
    );
}

async fn owner_snapshot(shared: &Arc<crate::services::discord::SharedData>) -> serde_json::Value {
    shared
        .mailbox(ChannelId::new(CHANNEL_ID))
        .v2_request(|slot| {
            let owner = slot.as_ref().ok_or("owner must be installed")?;
            serde_json::to_value(&owner.snapshot).map_err(|error| error.to_string())
        })
        .await
        .unwrap()
}

fn owner_marks(shared: &crate::services::discord::SharedData, message: u64) -> Vec<char> {
    let mut visible = std::collections::BTreeSet::new();
    for op in shared
        .turn_view_reconciler
        .ops()
        .into_iter()
        .filter(|op| op.target.message_id.get() == message)
    {
        if op.add {
            visible.insert(op.emoji);
        } else {
            visible.remove(&op.emoji);
        }
    }
    visible.into_iter().collect()
}

async fn owner_busy_receipts(active: bool, observed: bool) {
    let busy = owner_fixture().await;
    let rt = &busy.rt;
    let _path = owner_tmux(&busy.pane, &rt.h._env_lock);
    assert!(
        crate::services::claude_tui::hook_server::current_hook_endpoint().is_none(),
        "the isolated owner fixture starts without a hook endpoint"
    );
    assert!(rt.h.proxy.starts_with("http://127.0.0.1:"));
    let _endpoint = crate::services::claude_tui::hook_server::publish_hook_endpoint(format!(
        "{}/claude-hook",
        rt.h.proxy
    ));
    busy.pane.set(InjectPane::TRANSCRIPT, &format!("{}\n{}\n",
        serde_json::json!({"type":"user","uuid":"owner-native","message":{"content":"go"}}),
        serde_json::json!({"type":"assistant","uuid":"owner-answer","apiBlockIndex":0,"message":{"id":"owner-answer","content":[{"type":"text","text":"working"}]}})));
    assert!(
        crate::services::tui_turn_state::observe_claude_jsonl_turn_state(
            &busy.pane.path(InjectPane::TRANSCRIPT)
        )
        .is_busy(),
        "the native busy fixture must have an open transcript turn"
    );
    owner_install(rt, &busy.pane, active).await;
    let events = tempfile::tempdir().unwrap();
    let _events = BindingRoot::enter(events.path());
    let session = busy.pane.session();
    let _ready = observed.then(|| ReadyPane::mark(&session));
    let _confirmed = observed.then(|| {
        bind_turn_mode_transcript(
            &rt.h.shared,
            &ProviderKind::Claude,
            CHANNEL_ID,
            &session,
            &busy.pane.path(InjectPane::TRANSCRIPT),
        )
    });
    let presence = observed.then(|| {
        Arc::new(LiveSupervisor::new(
            rt.h.shared.clone(),
            ProviderKind::Claude,
            CHANNEL_ID,
        ))
    });
    assert_eq!(
        supervisor::observes(&ProviderKind::Claude, CHANNEL_ID),
        observed
    );
    if let Some(presence) = &presence {
        assert!(
            wait_until(WAIT, || {
                let presence = presence.clone();
                Box::pin(async move {
                    presence.tick().await;
                    presence.state()["activity"] == "busy"
                })
            })
            .await,
            "the real transcript reader must first observe Busy: {}",
            presence.state()
        );
    }
    let ids = [fresh_id(), fresh_id()];
    let pause = OwnerWakePause::new(ChannelId::new(CHANNEL_ID));
    owner_deliver(rt, ids[0], "owner front input").await;
    owner_deliver(rt, ids[1], "owner follower input").await;
    let queued = owner_snapshot(&rt.h.shared).await;
    assert_eq!(
        queued["items"].as_array().unwrap().len(),
        2,
        "owner queue before wake: {queued}; hooks={:?}; notices={:?}; tmux={:?}",
        ids.map(hook::seen),
        rt.notices(),
        busy.pane.read("log")
    );
    assert_eq!(queued["items"][0]["message_id"], ids[0]);
    assert_eq!(queued["items"][1]["message_id"], ids[1]);
    assert_eq!(
        rt.h.shared
            .mailbox(ChannelId::new(CHANNEL_ID))
            .v2_request(|slot| { Ok(slot.as_ref().unwrap().followers().len()) })
            .await
            .unwrap(),
        1,
        "the front claim must contain the second original as a merging follower"
    );
    assert_eq!(
        rt.h.shared
            .mailbox(ChannelId::new(CHANNEL_ID))
            .has_active_turn()
            .await
            .unwrap(),
        active
    );
    drop(pause);
    crate::services::discord::router::message_handler::owner_wake(
        &rt.h.shared,
        ChannelId::new(CHANNEL_ID),
    );
    assert!(
        wait_until(WAIT, || {
            let shared = rt.h.shared.clone();
            Box::pin(async move {
                let snapshot = owner_snapshot(&shared).await;
                snapshot["items"].as_array().unwrap().is_empty()
                    && snapshot["head_injection"].is_null()
                    && snapshot["retained_inputs"].as_array().unwrap().is_empty()
                    && ids.iter().all(|id| {
                        snapshot["terminal_identities"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|entry| entry["identity"]["primary"].as_u64() == Some(*id))
                    })
            })
        })
        .await,
        "the owner must settle both originals as delivered before checking their UI: snapshot={}; keys={:?}; hooks={:?}; tmux={:?}",
        owner_snapshot(&rt.h.shared).await,
        busy.pane.keys(),
        ids.map(hook::seen),
        busy.pane.read("log")
    );
    assert_eq!(
        busy.pane.keys(),
        ["paste-buffer", "send-keys"],
        "one actual paste and Enter for the merged unit"
    );
    let pasted = busy.pane.read("buffer").expect("the actual pasted frame");
    assert!(pasted.contains("owner front input") && pasted.contains("owner follower input"));
    assert_eq!(
        rt.h.provider_starts(),
        0,
        "the native busy pane must not start a provider process"
    );
    let reads = busy.pane.read("log").unwrap();
    assert!(
        reads.contains("list-panes") && reads.contains("#{pid} #{session_id}"),
        "the owner must observe the live execution and physical pane"
    );
    if !active {
        let ops = rt.h.shared.turn_view_reconciler.ops();
        assert!(
            ops.iter()
                .any(|op| op.target.message_id.get() == ids[0] && op.emoji == '⏳' && op.add)
        );
        assert!(
            ops.iter()
                .any(|op| op.target.message_id.get() == ids[0] && op.emoji == '⏳' && !op.add),
            "the initially idle owner turn must roll back before into_busy_pane submits"
        );
    }
    assert!(
        wait_until(WAIT, || {
            let shared = rt.h.shared.clone();
            Box::pin(async move { ids.iter().all(|id| owner_marks(&shared, *id) == ['➕']) })
        })
        .await,
        "a successful installed-owner busy write must mark every original with only plus"
    );
    if let Some(presence) = presence {
        let before_idle = rt.h.shared.turn_view_reconciler.ops().len();
        let mut transcript = std::fs::OpenOptions::new()
            .append(true)
            .open(busy.pane.path(InjectPane::TRANSCRIPT))
            .unwrap();
        writeln!(
            transcript,
            "{}",
            serde_json::json!({"type":"system","subtype":"turn_duration"})
        )
        .unwrap();
        busy.pane.idle();
        assert!(
            wait_until(WAIT, || {
                let presence = presence.clone();
                let shared = rt.h.shared.clone();
                Box::pin(async move {
                    presence.tick().await;
                    presence.state()["activity"] == "idle"
                        && ids
                            .iter()
                            .all(|id| owner_marks(&shared, *id) == ['✅', '➕'])
                })
            })
            .await,
            "real Idle must keep each owner receipt and add completion"
        );
        assert!(
            rt.h.shared
                .turn_view_reconciler
                .ops()
                .iter()
                .skip(before_idle)
                .filter(|op| ids.contains(&op.target.message_id.get()))
                .all(|op| op.add && op.emoji == '✅'),
            "Idle must never delete the owner receipt"
        );
    }
    rt.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reg_7832_owner_active_busy_marks_front_and_follower_pg() {
    owner_busy_receipts(true, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reg_7832_owner_native_busy_marks_front_and_follower_pg() {
    owner_busy_receipts(false, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reg_7832_owner_active_busy_observed_marks_front_and_follower_pg() {
    owner_busy_receipts(true, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reg_7832_owner_native_busy_observed_marks_front_and_follower_pg() {
    owner_busy_receipts(false, true).await;
}

async fn owner_refused_receipt(case: &'static str) {
    let busy = owner_fixture().await;
    let rt = &busy.rt;
    let _path = owner_tmux(&busy.pane, &rt.h._env_lock);
    owner_install(rt, &busy.pane, true).await;
    match case {
        "held" => busy.pane.menu(),
        "uncertain" => busy.pane.set("fail_paste", ""),
        "not_sent" => {}
        _ => unreachable!(),
    }
    let message = fresh_id();
    let text = if case == "not_sent" {
        "x".repeat(crate::services::claude_tui::busy_inject::MAX_INPUT_BYTES)
    } else {
        format!("owner {case} input")
    };
    owner_deliver(rt, message, &text).await;
    assert!(
        wait_until(WAIT, || {
            let shared = rt.h.shared.clone();
            let messages = rt.h.mock.messages.clone();
            Box::pin(async move {
                let snapshot = owner_snapshot(&shared).await;
                match case {
                    "not_sent" => snapshot["retained_inputs"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|entry| entry["reason"]["kind"] == "rejected"),
                    "uncertain" => snapshot["head_injection"]["phase"] == "unknown",
                    "held" => {
                        snapshot["head_injection"].is_null()
                            && snapshot["items"].as_array().unwrap().len() == 1
                            && messages
                                .lock()
                                .unwrap()
                                .values()
                                .any(|(_, notice)| notice.contains("모달"))
                    }
                    _ => false,
                }
            })
        })
        .await,
        "the installed owner must finish the expected refusal before checking its UI: case={case}; state={}; keys={:?}; hook={:?}; notices={:?}; tmux={:?}",
        owner_snapshot(&rt.h.shared).await,
        busy.pane.keys(),
        hook::seen(message),
        rt.notices(),
        busy.pane.read("log")
    );
    let snapshot = owner_snapshot(&rt.h.shared).await;
    assert!(
        snapshot["terminal_identities"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        owner_marks(&rt.h.shared, message)
            .iter()
            .all(|emoji| *emoji != '➕'),
        "a refused or uncertain write has no successful injection receipt"
    );
    assert!(
        busy.pane.keys().is_empty(),
        "no successful paste or Enter in these refusal fixtures"
    );
    rt.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reg_7832_owner_not_sent_and_held_have_no_receipt_pg() {
    owner_refused_receipt("not_sent").await;
    owner_refused_receipt("held").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reg_7832_owner_uncertain_write_has_no_receipt_pg() {
    owner_refused_receipt("uncertain").await;
}
