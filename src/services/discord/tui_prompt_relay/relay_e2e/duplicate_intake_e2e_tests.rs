use super::*;
use crate::services::discord::queue_io::input_ingress as ingress;
use crate::services::discord::runtime_store;
use crate::services::discord::runtime_store::durable_probe::{Plan, Step, Watch};

const FIRST: &str = "핸드오프 첫 메시지";
const SECOND: &str = "핸드오프 다른 메시지";

async fn fixture() -> Busy {
    let busy = owner_fixture_with_health(true).await;
    busy.rt
        .h
        .health_registry
        .as_ref()
        .unwrap()
        .set_utility_bot_user_id_for_tests(UtilityBotRole::Announce, ANNOUNCE_ID)
        .await;
    busy
}

fn native_busy(pane: &InjectPane) {
    pane.set(InjectPane::TRANSCRIPT, &format!("{}\n{}\n",
        serde_json::json!({"type":"user","uuid":"owner-native","message":{"content":"go"}}),
        serde_json::json!({"type":"assistant","uuid":"owner-answer","apiBlockIndex":0,"message":{"id":"owner-answer","content":[{"type":"text","text":"working"}]}})));
}

async fn live(rt: &Runtime, message: u64, text: &str) {
    let mut sent = user_message(message, text);
    sent.author.id = serenity::UserId::new(ANNOUNCE_ID);
    sent.author.bot = true;
    let event = serenity::FullEvent::Message { new_message: sent };
    let (ctx, data) = (rt.h.ctx.clone(), rt.h.clone_data());
    tokio::time::timeout(
        WAIT,
        tokio::spawn(async move {
            ingress::ADOPTS
                .scope(true, super::super::super::boxed_intake(&ctx, &event, &data))
                .await
        }),
    )
    .await
    .unwrap()
    .unwrap()
    .unwrap();
}

fn history(rt: &Runtime, anchor: u64, message: u64) {
    rt.h.seed_channel_history(&[(anchor, "previous answer", true), (message, FIRST, false)]);
    let mut entries = rt.h.mock.history.lock().unwrap();
    let message_id = message.to_string();
    let entry = entries
        .iter_mut()
        .find(|entry| entry["id"].as_str() == Some(message_id.as_str()))
        .unwrap();
    entry["author"]["id"] = serde_json::json!(ANNOUNCE_ID.to_string());
    entry["author"]["bot"] = serde_json::json!(true);
}

/// The old intake cursor and phase-1 response precede this message; phase 2 alone finds it.
async fn scan(rt: &Runtime, anchor: u64, message: u64, live_first: Option<&Busy>) {
    rt.h.seed_channel_history(&[(anchor, "previous answer", true)]);
    runtime_store::save_last_message_id("claude", CHANNEL_ID, message);
    rt.h.seed_scanner_checkpoint(anchor);
    rt.h.shared.last_message_ids.insert(rt.h.channel_id, anchor);
    rt.h.hold_next_history();
    let (entered, arrived) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel();
    let pause = live_first.map(|_| (entered, released));
    let (http, shared) = (rt.h.ctx.http.clone(), rt.h.shared.clone());
    let sweep = tokio::spawn(async move {
        ingress::BEFORE_RESUME
            .scope(
                std::cell::RefCell::new(pause),
                crate::services::discord::catch_up::catch_up_missed_messages(
                    &http,
                    &shared,
                    &ProviderKind::Claude,
                ),
            )
            .await
    });
    assert!(rt.h.wait_for_held_history(WAIT).await);
    rt.h.hold_next_history();
    rt.h.release_held_history();
    assert!(rt.h.wait_for_held_history(WAIT).await);
    assert_eq!(rt.h.history_queries().last().unwrap().limit, Some(20));
    history(rt, anchor, message);
    rt.h.release_held_history();
    if let Some(busy) = live_first {
        tokio::time::timeout(WAIT, arrived).await.unwrap().unwrap();
        live(rt, message, FIRST).await;
        delivered(busy, &[message]).await;
        restart(rt).await;
        release.send(()).unwrap();
    }
    tokio::time::timeout(WAIT, sweep).await.unwrap().unwrap();
    assert!(
        !rt.h
            .shared
            .catch_up_retry_pending
            .contains_key(&rt.h.channel_id),
        "an already committed message must not schedule a persistence retry"
    );
    assert_eq!(
        rt.h.checkpoint(),
        Some(message),
        "a committed duplicate must retire the scan without a persistence retry"
    );
}

async fn restart(rt: &Runtime) {
    // Re-adopt the persisted queue after delivery: only its receipt, not its original, remains.
    let path = runtime_store::discord_pending_queue_root()
        .unwrap()
        .join("claude")
        .join(&rt.h.shared.token_hash)
        .join(format!("{CHANNEL_ID}.json"));
    rt.h.shared
        .mailbox(rt.h.channel_id)
        .v2_adopt(path, format!("claude/{}", rt.h.shared.token_hash))
        .await
        .unwrap();
}

async fn delivered(busy: &Busy, messages: &[u64]) {
    assert!(
        wait_until(WAIT, || {
            let shared = busy.rt.h.shared.clone();
            let messages = messages.to_vec();
            Box::pin(async move {
                let state = owner_snapshot(&shared).await;
                messages.iter().all(|message| {
                    state["terminal_identities"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|entry| entry["identity"]["primary"].as_u64() == Some(*message))
                }) && state["items"].as_array().unwrap().is_empty()
                    && state["head_injection"].is_null()
            })
        })
        .await,
        "the real owner must deliver each message: {}",
        owner_snapshot(&busy.rt.h.shared).await
    );
}

fn no_warning(rt: &Runtime) {
    assert_eq!(
        rt.notices()
            .iter()
            .filter(|text| text.as_str() == QUEUE_UNKNOWN)
            .count(),
        0,
        "a duplicate must never post a queue persistence warning: {:?}",
        rt.notices()
    );
}

async fn order(catch_up_first: bool) {
    let busy = fixture().await;
    let rt = &busy.rt;
    let _path = owner_tmux(&busy.pane, &rt.h._env_lock);
    let _endpoint = crate::services::claude_tui::hook_server::publish_hook_endpoint(format!(
        "{}/claude-hook",
        rt.h.proxy
    ));
    rt.h.shared
        .settings
        .write()
        .await
        .allowed_bot_ids
        .push(ANNOUNCE_ID);
    rt.h.register_channel_in_role_map();
    native_busy(&busy.pane);
    owner_install(rt, &busy.pane, true).await;
    let (anchor, message) = (fresh_id(), fresh_id());
    if catch_up_first {
        scan(rt, anchor, message, None).await;
    } else {
        scan(rt, anchor, message, Some(&busy)).await;
    }
    delivered(&busy, &[message]).await;
    assert_eq!(busy.pane.keys(), ["paste-buffer", "send-keys"]);
    let frame = busy.pane.read("buffer").unwrap();
    assert_eq!(frame.matches(FIRST).count(), 1);
    if catch_up_first {
        restart(rt).await;
        live(rt, message, FIRST).await;
    }
    let state = owner_snapshot(&rt.h.shared).await;
    assert!(
        state["items"].as_array().unwrap().is_empty(),
        "a duplicate must leave no queued copy before cleanup: {state}"
    );
    assert!(state["head_injection"].is_null(), "{state}");
    assert!(
        state["retained_inputs"].as_array().unwrap().is_empty(),
        "{state}"
    );
    let terminals = state["terminal_identities"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| entry["identity"]["primary"].as_u64() == Some(message))
        .count();
    assert_eq!(terminals, 1, "a duplicate must settle only once: {state}");
    no_warning(rt);
    assert_eq!(
        busy.pane.keys(),
        ["paste-buffer", "send-keys"],
        "the duplicate must not reach the pane"
    );
    assert_eq!(busy.pane.read("buffer").unwrap(), frame);
    assert_eq!(
        busy.pane.records().join("\n").matches(FIRST).count(),
        1,
        "the pane must record the input only once before cleanup"
    );
    assert!(
        rt.h.unhandled_requests().is_empty(),
        "{:?}",
        rt.h.unhandled_requests()
    );
    rt.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn catch_up_then_live_delivers_once_without_warning_pg() {
    order(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_then_catch_up_delivers_once_without_warning_pg() {
    order(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn distinct_messages_on_one_channel_both_reach_the_pane_pg() {
    let busy = fixture().await;
    let rt = &busy.rt;
    let _path = owner_tmux(&busy.pane, &rt.h._env_lock);
    let _endpoint = crate::services::claude_tui::hook_server::publish_hook_endpoint(format!(
        "{}/claude-hook",
        rt.h.proxy
    ));
    rt.h.shared
        .settings
        .write()
        .await
        .allowed_bot_ids
        .push(ANNOUNCE_ID);
    native_busy(&busy.pane);
    owner_install(rt, &busy.pane, true).await;
    let messages = [fresh_id(), fresh_id()];
    live(rt, messages[0], FIRST).await;
    delivered(&busy, &messages[..1]).await;
    std::fs::remove_file(busy.pane.path("pasted")).unwrap();
    live(rt, messages[1], SECOND).await;
    delivered(&busy, &messages).await;
    let frame = busy.pane.records().join("\n");
    for text in [FIRST, SECOND] {
        assert_eq!(frame.matches(text).count(), 1, "{frame}");
    }
    no_warning(rt);
    rt.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_real_queue_write_failure_posts_one_warning_pg() {
    let busy = fixture().await;
    let rt = &busy.rt;
    let _path = owner_tmux(&busy.pane, &rt.h._env_lock);
    rt.h.shared
        .settings
        .write()
        .await
        .allowed_bot_ids
        .push(ANNOUNCE_ID);
    native_busy(&busy.pane);
    owner_install(rt, &busy.pane, true).await;
    let path = runtime_store::discord_pending_queue_root()
        .unwrap()
        .join("claude")
        .join(&rt.h.shared.token_hash)
        .join(format!("{CHANNEL_ID}.json"));
    let watch = Watch::new(
        &path,
        Plan {
            fail: vec![Step::Write],
            ..Default::default()
        },
    );
    live(rt, fresh_id(), FIRST).await;
    assert!(
        watch
            .events()
            .iter()
            .any(|event| event.step == Step::Write && !event.ok),
        "the real queue write must reach the injected failure"
    );
    assert_eq!(
        rt.notices()
            .iter()
            .filter(|text| text.as_str() == QUEUE_UNKNOWN)
            .count(),
        1,
        "{:?}",
        rt.notices()
    );
    assert!(busy.pane.keys().is_empty());
    drop(watch);
    rt.finish().await;
}
