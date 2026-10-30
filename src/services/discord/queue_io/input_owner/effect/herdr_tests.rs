//! The owner's busy Claude effect on a real Herdr socket rig: the logical session names the
//! history, only the bound physical pane takes the paste and Enter, and a failure reports once.
use super::*;
use crate::db::dispatched_sessions::hosted_execution::{
    HostedExecution, HostedLocation, HostedOwner, HostedState, SourceRef,
};
use crate::services::discord::queue_io::input_owner::effect::PreparedPayload;
use crate::services::discord::queue_io::input_owner::pump;
use crate::services::herdr_admission::{Admission, force_for_test};
use crate::services::session_host::herdr_socket_rig_tests::{HerdrRig, KEY, NODE, PANE, SESSION};
use crate::services::turn_orchestrator::{ChannelMailboxRegistry, PendingQueueItem};
use serde_json::{Value, json};
use std::io::Write;
use std::sync::Arc;

const PROMPT: &str = "owner herdr question";
const BUSY: &str = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"go\"}}\n\
    {\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"working\"}]}}\n";

/// The busy Claude screen with an empty composer, as Herdr reads the pane.
fn busy_screen() -> Value {
    draft_screen("")
}

/// The busy Claude screen with `draft` typed in its composer.
fn draft_screen(draft: &str) -> Value {
    let border = "─".repeat(60);
    let text = format!(
        "⏺ Working on it.\n\n✻ Thinking… (12s · esc to interrupt)\n\n{border}\n❯\u{00a0}{draft}\n{border}\n  ⏵⏵ bypass permissions on (shift+tab to cycle)"
    );
    read(&text)
}

fn read(text: &str) -> Value {
    json!({"type": "pane_read", "read": {
        "pane_id": PANE, "workspace_id": "w1", "tab_id": "w1:1", "source": "recent_unwrapped",
        "format": "text", "text": text, "revision": 3, "truncated": false
    }})
}

/// A Bound Claude execution `nonce` on the rig's pane, owned by the logical session `logical`.
fn bound(rig: &HerdrRig, channel: u64, logical: &str, nonce: &str) -> HostedExecution {
    let owner = HostedOwner {
        provider: "claude".into(),
        discord_token_hash: "owner-herdr".into(),
        channel_id: channel.to_string(),
        logical_key: logical.into(),
        owner_node: NODE.into(),
        runtime_root: "/adk/runtime".into(),
    };
    HostedExecution {
        schema: 1,
        state: HostedState::Bound,
        execution_nonce: nonce.into(),
        location: Some(HostedLocation {
            host: "herdr".into(),
            execution_node: NODE.into(),
            endpoint_config_key: KEY.into(),
            socket_addr: rig.socket().display().to_string(),
            named_session: SESSION.into(),
            pane_id: PANE.into(),
        }),
        expected: Some(rig.expected(nonce)),
        source_ref: SourceRef {
            runtime_root: owner.runtime_root.clone(),
            channel: owner.channel_id.clone(),
            provider: "claude".into(),
            logical_key: logical.into(),
            execution_nonce: nonce.into(),
            initial_source: None,
            baseline_event_seq: None,
        },
        owner,
    }
}

/// What one owner attempt left: the rig, the completion, the owner's snapshot, the nonce, and
/// the head as it stood when ctrl+s reached the pane.
type Attempted = (
    HerdrRig,
    Completion,
    Value,
    String,
    Arc<std::sync::Mutex<Option<Value>>>,
);

/// One owner attempt on a Herdr pane showing `screen` whose logical session is not its pane id;
/// `logged` makes Claude record the pasted frame on Enter, and `writer` is the pane the owner binds.
fn attempt(channel: u64, logged: bool, writer: Option<&str>, screen: Value) -> Attempted {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _hosts = crate::config::session_hosts::force_for_test(Some(NODE), &[(channel, NODE)]);
    let rig = HerdrRig::start();
    let _registry = rig.registry_on_this_thread();
    let execution = uuid::Uuid::new_v4().simple().to_string();
    rig.run_provider(&rig.context(&execution), false);
    let logical = format!("AgentDesk-claude-owner-{channel}");
    let marker = crate::services::tmux_common::session_temp_path(&logical, "host_kind");
    std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
    std::fs::write(marker, "herdr").unwrap();
    let target =
        crate::services::claude::herdr_turn::gate(&bound(&rig, channel, &logical, &execution))
            .unwrap();
    rig.answer("pane.read", screen);
    let dir = tempfile::tempdir().unwrap();
    let transcript = dir.path().join("busy.jsonl");
    std::fs::write(&transcript, BUSY).unwrap();
    let queue = crate::services::discord::runtime_store::discord_pending_queue_root()
        .unwrap()
        .join("claude/input-owner-herdr");
    let stashed = Arc::new(std::sync::Mutex::new(None));
    {
        let path = transcript.clone();
        let mut pasted = String::new();
        let (file, stash) = (queue.join(format!("{channel}.json")), stashed.clone());
        rig.on_send(move |params| {
            if params["keys"] == json!(["ctrl+s"]) {
                let raw = std::fs::read_to_string(&file).unwrap_or_default();
                let owner: Value = serde_json::from_str(&raw).unwrap_or_default();
                *stash.lock().unwrap() = Some(owner["head_injection"].clone());
            }
            if !logged {
                return;
            }
            if let Some(text) = params["text"].as_str() {
                let text = text.trim_start_matches("\u{1b}[200~");
                pasted = text.trim_end_matches("\u{1b}[201~").to_string();
                return;
            }
            if params["keys"] != json!(["enter"]) {
                return;
            }
            let record = json!({"type": "queue-operation", "operation": "enqueue",
                "content": pasted, "sessionId": "5340", "timestamp": "2026-10-15T00:00:00.000Z"});
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            writeln!(file, "{record}").unwrap();
        });
    }
    // Every thread admits Herdr input, as an unstopped node does.
    let open = || {
        let stop = std::env::temp_dir().join(format!("adk-oh-none-{}", uuid::Uuid::new_v4()));
        std::mem::forget(force_for_test(Admission::new(None, Some(stop))));
    };
    open();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .on_thread_start(open)
        .build()
        .unwrap();
    let _entered = runtime.enter();
    let registry = ChannelMailboxRegistry::default();
    let mailbox = registry.handle(poise::serenity_prelude::ChannelId::new(channel));
    let id = 8_000_000_000_000_007_000 + channel % 1000;
    let original: PendingQueueItem = serde_json::from_value(json!({"author_id":7,
        "message_id":id,"channel_id":channel,"source_message_ids":[id],
        "queued_generation":1,"text":PROMPT}))
    .unwrap();
    let nonce = format!("{:032x}", channel);
    let frame = busy_inject::frame("discord", "7", "tester", &nonce, PROMPT);
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(&transcript).unwrap();
    let prepared = PreparedPayload {
        frame,
        sources: vec![id.to_string()],
        attempt: json!({"nonce":nonce,"provider":"claude",
            "target":{"host":"herdr","owner":logical,"root":rig.socket().display().to_string(),
                "execution_nonce":execution},
            "source":{"kind":"existing","session_id":"sid-herdr",
                "transcript":transcript.display().to_string(),
                "file":[meta.dev(), meta.ino()],"offset":meta.len()}}),
    };
    let physical = format!("{}#{PANE}", rig.socket().display());
    let leaf = BusyClaudeHerdr {
        target,
        writer: writer.map_or(physical, str::to_string),
        session: logical,
        transcript,
        channel,
        source: "discord".into(),
        author: "7".into(),
        name: "tester".into(),
        nonce: nonce.clone(),
        text: PROMPT.into(),
    }
    .leaf();
    let completion = runtime.block_on(async {
        std::fs::create_dir_all(&queue).unwrap();
        mailbox
            .v2_adopt(queue.join(format!("{channel}.json")), "input-owner".into())
            .await
            .unwrap();
        mailbox
            .v2_request(move |s| {
                s.as_mut()
                    .unwrap()
                    .enqueue_fixture(&original)
                    .map_err(|e| format!("{e:?}"))
            })
            .await
            .unwrap();
        pump::submit(&mailbox, prepared, Arc::new(|| 10_000), leaf).await
    });
    let snapshot = runtime
        .block_on(mailbox.v2_request(|s| s.as_ref().unwrap().json()))
        .unwrap();
    (rig, completion.unwrap(), snapshot, nonce, stashed)
}

fn alerts(nonce: &str) -> usize {
    crate::services::observability::events::recent(10_000)
        .iter()
        .filter(|e| e.event_type == "busy_inject_unconfirmed" && e.payload["nonce"] == nonce)
        .count()
}

/// The owner pastes the reserved frame once and enters once on the bound pane; the history
/// registration names the logical session, and the delivered head leaves custody.
#[test]
fn owner_herdr_busy_claude_writes_the_bound_pane_once() {
    let (rig, completion, snapshot, nonce, _) = attempt(7_272_531, true, None, busy_screen());
    assert_eq!(completion, Completion::Submitted, "{snapshot}");
    let sends = rig.sends();
    assert_eq!(sends.len(), 2, "{sends:?}");
    let frame = busy_inject::frame("discord", "7", "tester", &nonce, PROMPT);
    assert_eq!(sends[0]["text"], format!("\u{1b}[200~{frame}\u{1b}[201~"));
    assert_eq!(sends[1]["keys"], json!(["enter"]));
    assert!(snapshot["head_injection"].is_null(), "{snapshot}");
    assert_eq!(alerts(&nonce), 0);
}

/// An applied paste and Enter that Claude never records is still delivered: the head settles and
/// nothing is retained, with no alert, no second send and no read after the first key.
#[test]
fn owner_herdr_enter_without_receipt_is_delivered_once() {
    let (rig, completion, snapshot, nonce, _) = attempt(7_272_532, false, None, busy_screen());
    assert_eq!(completion, Completion::Submitted, "{snapshot}");
    assert_eq!(rig.sends().len(), 2, "{:?}", rig.sends());
    assert_eq!(reads_after_first_key(&rig), 0);
    std::thread::sleep(busy_inject::TIMING.confirm_window);
    assert_eq!(alerts(&nonce), 0);
    assert!(snapshot["head_injection"].is_null(), "{snapshot}");
    assert_eq!(snapshot["retained_inputs"], json!([]), "{snapshot}");
}

/// An owner bound to another pane gets no byte in: the fence's refusal after the payload started
/// is a failed paste, so the head stays Unknown with one alert and nothing is sent again.
#[test]
fn owner_herdr_foreign_pane_gets_no_content() {
    let foreign = Some("/elsewhere#w1-9");
    let (rig, completion, snapshot, nonce, _) = attempt(7_272_533, true, foreign, busy_screen());
    assert_eq!(
        completion,
        Completion::WriteUncertain("paste_failed".into())
    );
    assert!(rig.sends().is_empty(), "{:?}", rig.sends());
    assert_eq!(alerts(&nonce), 1);
    assert_eq!(snapshot["head_injection"]["phase"], "unknown", "{snapshot}");
    assert_eq!(snapshot["retained_inputs"], json!([]), "{snapshot}");
}

/// Pane reads that reached the server after its first write.
fn reads_after_first_key(rig: &HerdrRig) -> usize {
    let requests = rig.requests();
    let method = |r: &Value| r["method"].as_str().unwrap_or_default().to_string();
    let first = requests
        .iter()
        .position(|r| method(r).starts_with("pane.send"));
    first.map_or(0, |at| {
        (requests[at..].iter())
            .filter(|r| method(r) == "pane.read")
            .count()
    })
}

/// A visible draft is parked with Herdr's ctrl+s before the payload starts, whichever channel the
/// input names; one paste and one Enter follow, and nothing reads the pane after the first key.
#[test]
fn owner_herdr_parks_a_visible_draft_before_the_payload_starts() {
    let (rig, completion, snapshot, nonce, stashed) =
        attempt(7_272_534, true, None, draft_screen("half typed"));
    assert_eq!(completion, Completion::Submitted, "{snapshot}");
    let sends = rig.sends();
    assert_eq!(sends.len(), 3, "{sends:?}");
    assert_eq!(sends[0]["keys"], json!(["ctrl+s"]));
    let frame = busy_inject::frame("discord", "7", "tester", &nonce, PROMPT);
    assert_eq!(sends[1]["text"], format!("\u{1b}[200~{frame}\u{1b}[201~"));
    assert_eq!(sends[2]["keys"], json!(["enter"]));
    let at_stash = stashed
        .lock()
        .unwrap()
        .clone()
        .expect("ctrl+s reached the pane");
    assert_eq!(
        at_stash["payload_window"]["kind"], "bootstrap",
        "start after C-s: {at_stash}"
    );
    assert_eq!(reads_after_first_key(&rig), 0);
    assert!(snapshot["head_injection"].is_null(), "{snapshot}");
}

/// A modal on the pane gets no key: the front is held, unretained, for the release wake.
#[test]
fn owner_herdr_modal_holds_the_front_without_a_key() {
    let modal =
        read("Do you want to proceed?\n❯ 1. Yes\n  2. No\nEnter to select · Esc to cancel\n");
    let (rig, completion, snapshot, _, stashed) = attempt(7_272_535, true, None, modal);
    assert_eq!(completion, Completion::NotSent(MODAL.into()), "{snapshot}");
    assert!(rig.sends().is_empty(), "{:?}", rig.sends());
    assert!(stashed.lock().unwrap().is_none());
    assert!(snapshot["head_injection"].is_null(), "{snapshot}");
    assert_eq!(snapshot["retained_inputs"], json!([]), "{snapshot}");
    assert_eq!(snapshot["items"][0]["text"], PROMPT, "{snapshot}");
}

/// One busy Claude screen per composer case, and whether the owner parks it with C-s.
fn composer_cases() -> [(&'static str, String, usize); 5] {
    let border = "─".repeat(60);
    let thinking = "⏺ Working on it.\n\n✻ Thinking… (12s · esc to interrupt)\n";
    let screen = |above: &str, composer: &str| {
        format!(
            "{thinking}{above}\n{border}\n❯\u{00a0}{composer}\n{border}\n  ⏵⏵ bypass permissions on (shift+tab to cycle)"
        )
    };
    [
        ("person draft", screen("", "half typed"), 1),
        (
            "injected frame",
            screen("", "[📱 tester] earlier question"),
            0,
        ),
        ("existing stash", screen("› stashed", "half typed"), 0),
        ("empty composer", screen("", ""), 0),
        ("unreadable", thinking.into(), 0),
    ]
}

/// Herdr and tmux owners read the same capture alike: only a person's draft is parked with C-s,
/// never an injected frame, a draft beside a stash, an empty or an unread composer.
#[test]
fn the_tmux_owner_parks_only_what_the_herdr_owner_parks() {
    for (at, (case, screen, parks)) in composer_cases().into_iter().enumerate() {
        let (rig, completion, snapshot, _, _) =
            attempt(7_272_548 + at as u64, true, None, read(&screen));
        assert_eq!(
            completion,
            Completion::Submitted,
            "herdr {case}: {snapshot}"
        );
        let sends = rig.sends();
        let keyed = |key: &str| sends.iter().filter(|s| s["keys"] == json!([key])).count();
        assert_eq!(keyed("ctrl+s"), parks, "herdr {case}: {sends:?}");
        assert_eq!(
            (sends.len(), keyed("enter")),
            (parks + 2, 1),
            "herdr {case}: {sends:?}"
        );
    }
    use crate::services::claude_tui::input::submission::tests::ScriptedTmux;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let tmux = ScriptedTmux::new("");
    let dir = tempfile::tempdir().unwrap();
    let transcript = dir.path().join("busy.jsonl");
    std::fs::write(&transcript, BUSY).unwrap();
    for (at, (case, screen, parks)) in composer_cases().into_iter().enumerate() {
        tmux.clear();
        tmux.show(&screen);
        let (session, nonce) = (format!("owner-park-{at}"), format!("0a1b2c3{at}"));
        let pane = busy_inject::Pane::with_program(&session, "tmux".into());
        let request = busy_inject::Request {
            session: &session,
            transcript: &transcript,
            source: "discord",
            author: "7",
            name: "tester",
            nonce: &nonce,
            text: PROMPT,
        };
        let report = busy_inject::inject_owned(&pane, &request, &busy_inject::TIMING);
        assert_eq!(
            report.outcome,
            busy_inject::Outcome::Injected,
            "tmux {case}"
        );
        let sends = tmux.sends();
        let keyed = |key: &str| sends.iter().filter(|s| *s == key).count();
        assert_eq!(keyed("C-s"), parks, "tmux {case}: {sends:?}");
        assert_eq!(
            (keyed("paste"), keyed("Enter")),
            (1, 1),
            "tmux {case}: {sends:?}"
        );
        let calls = tmux.pane_calls(&session);
        let first = calls
            .iter()
            .position(|c| c == "send-keys" || c == "paste-buffer");
        let read = calls[..first.unwrap()]
            .iter()
            .filter(|c| *c == "capture-pane");
        assert_eq!(
            read.count(),
            1,
            "tmux {case}: one read before any key: {calls:?}"
        );
    }
}
