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
    let border = "─".repeat(60);
    let text = format!(
        "⏺ Working on it.\n\n✻ Thinking… (12s · esc to interrupt)\n\n{border}\n❯\u{00a0}\n{border}\n  ⏵⏵ bypass permissions on (shift+tab to cycle)"
    );
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

/// One owner attempt on a busy Herdr pane whose logical session is not its pane id; `logged`
/// makes Claude record the pasted frame on Enter, and `writer` is the pane the owner binds.
fn attempt(
    channel: u64,
    logged: bool,
    writer: Option<&str>,
) -> (HerdrRig, Completion, Value, String) {
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
    rig.answer("pane.read", busy_screen());
    let dir = tempfile::tempdir().unwrap();
    let transcript = dir.path().join("busy.jsonl");
    std::fs::write(&transcript, BUSY).unwrap();
    if logged {
        let path = transcript.clone();
        let mut pasted = String::new();
        rig.on_send(move |params| {
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
        let queue = crate::services::discord::runtime_store::discord_pending_queue_root()
            .unwrap()
            .join("claude/input-owner-herdr");
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
    (rig, completion.unwrap(), snapshot, nonce)
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
    let (rig, completion, snapshot, nonce) = attempt(7_272_531, true, None);
    assert_eq!(completion, Completion::Submitted, "{snapshot}");
    let sends = rig.sends();
    assert_eq!(sends.len(), 2, "{sends:?}");
    let frame = busy_inject::frame("discord", "7", "tester", &nonce, PROMPT);
    assert_eq!(sends[0]["text"], format!("\u{1b}[200~{frame}\u{1b}[201~"));
    assert_eq!(sends[1]["keys"], json!(["enter"]));
    assert!(snapshot["head_injection"].is_null(), "{snapshot}");
    assert_eq!(alerts(&nonce), 0);
}

/// A paste Claude never records stays unconfirmed with the one alert health raised; the owner
/// neither raises another nor sends again, and the head stays Unknown.
#[test]
fn owner_herdr_unconfirmed_alerts_once_and_never_resends() {
    let (rig, completion, snapshot, nonce) = attempt(7_272_532, false, None);
    assert_eq!(
        completion,
        Completion::WriteUncertain("not_observed".into())
    );
    assert_eq!(rig.sends().len(), 2, "{:?}", rig.sends());
    assert_eq!(alerts(&nonce), 1);
    assert_eq!(snapshot["head_injection"]["phase"], "unknown", "{snapshot}");
}

/// An owner bound to another pane writes nothing to this one: the gate's content fence refuses
/// before any byte. Health reads that refusal inside the send as a failed paste, so the attempt
/// stays Unknown with health's one alert and is never sent again.
#[test]
fn owner_herdr_foreign_pane_gets_no_content() {
    let (rig, completion, snapshot, nonce) = attempt(7_272_533, true, Some("/elsewhere#w1-9"));
    assert_eq!(
        completion,
        Completion::WriteUncertain("paste_failed".into())
    );
    assert!(rig.sends().is_empty(), "{:?}", rig.sends());
    assert_eq!(alerts(&nonce), 1);
    assert_eq!(snapshot["head_injection"]["phase"], "unknown", "{snapshot}");
    assert_eq!(snapshot["retained_inputs"], json!([]), "{snapshot}");
}
