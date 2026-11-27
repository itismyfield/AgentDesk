//! The session a thread turn selects, read where the intake prepares it.

use super::REASON;
use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
use crate::services::claude::{fresh_notice, tui_session_launch::ResumePane};
use crate::services::discord::prompt_builder::{DispatchProfile, load_channel_recent_context};
use crate::services::discord::router::message_handler::intake_turn::session_preparation as prep;
use crate::services::platform::tmux::PaneLiveness::{self, DeadOrAbsent, Live, ProbeError};
use crate::services::provider::ProviderKind::{self, Claude, Codex};
use crate::services::session_host::surface_policy::VerifiedSurface;
use crate::services::tmux_diagnostics::PaneLivenessOverrideGuard as Pane;
use crate::services::tui_o::channel_policy::{Adoption, BootChannels};
use crate::services::tui_o::cutover::{claude_thread, test_override};
use crate::services::tui_o::shadow::binding_reader::source_id_for;
use crate::services::tui_prompt_dedupe::binding_events as p5;
use poise::serenity_prelude::ChannelId;
use serde_json::json;

const NATIVE: &str = "8372aaaa-0000-4000-8000-000000000001";
const TMUX: &str = "AgentDesk-claude-routine-process-8372";
const PARENT: u64 = 8_372_100;

/// `thread` as boot leaves a thread found under a late-adopt Claude TUI `parent`.
pub(in crate::services::discord) fn thread_child(
    thread: u64,
    parent: u64,
) -> test_override::ChannelsGuard {
    let channels = json!({"claude": {"id": parent.to_string(), "runtime": "tui"}});
    let writer = json!({"channels": [parent], "late_adopt": true});
    let config: crate::config::Config = serde_json::from_value(json!({
        "server": {}, "agents": [{"id": "c", "name": "C", "channels": channels}],
        "tui_o": {"writer": writer, "turn": {"channels": [parent]}}
    }))
    .unwrap();
    let pending = |_: &_, _| {
        Ok(std::collections::BTreeMap::from([(
            parent,
            Adoption::Pending,
        )]))
    };
    let boot = BootChannels::validate(&config).unwrap();
    let boot = boot.seeded_for_test(&config, pending).unwrap();
    let ids = (thread.to_string(), parent.to_string());
    let surface = json!({"id": ids.0, "parent_id": ids.1, "type": 11, "name": "t"});
    let surface = serde_json::from_value(surface).unwrap();
    boot.reserve_boot_children(&[(thread, parent)]);
    let surface = VerifiedSurface::from_discord_for_test(&surface, ClaudeTui).unwrap();
    assert!(boot.resolve_child(thread, surface, false));
    test_override::force_boot(boot)
}

/// One way a turn differs from a person's reply to a routine's process session on a gone pane.
enum Turn {
    Reply,
    By(ProviderKind),
    Pane(Option<PaneLiveness>),
    /// What the thread's durable files say about `NATIVE`.
    History(&'static str),
    Herdr,
    NoThread,
}

fn write_history(root: &std::path::Path, channel: u64, history: &str) -> std::path::PathBuf {
    let transcript = root.join(format!("{channel}.jsonl"));
    std::fs::write(&transcript, "{}\n").unwrap();
    let source = source_id_for(NATIVE, &transcript).unwrap();
    let evidence = json!({"hook_event": "UserPromptSubmit", "received_at": chrono::Utc::now()});
    let verified = json!({"seq": 1, "channel_id": channel, "provider": "claude",
        "tmux_session": TMUX, "execution_nonce": "n", "old": null, "new": {"source": source},
        "cause": "same_session_verified", "parent_hint": null, "evidence": evidence,
        "committed_at": chrono::Utc::now(), "verified": true});
    let resume =
        json!({"parent": PARENT, "tmux": TMUX, "source": source, "offset": 0, "sha256": ""});
    let (dir, file, body) = match history {
        "verified" => (p5::BINDING_EVENTS_DIR, "log", verified),
        "unreadable" => (p5::BINDING_EVENTS_DIR, "log", json!("not a binding record")),
        "reserved" => ("runtime/claude_thread_resumes", "json", json!([resume])),
        _ => return transcript,
    };
    std::fs::create_dir_all(root.join(dir)).unwrap();
    let path = root.join(dir).join(format!("{channel}.{file}"));
    std::fs::write(path, format!("{body}\n")).unwrap();
    transcript
}

// Only a Claude turn headed for a gone pane in a reserved thread, whose stored session nothing
// verified, is started fresh; it then carries recent messages and is no longer a refused resume.
#[tokio::test]
async fn unverified_thread_cold_resume_starts_fresh() {
    let cases = [
        ("reply to a routine's process session", Turn::Reply),
        ("input joining a live pane", Turn::Pane(Some(Live))),
        ("pane that could not be read", Turn::Pane(Some(ProbeError))),
        ("turn without a pane", Turn::Pane(None)),
        ("Codex turn in a Claude thread", Turn::By(Codex)),
        ("hook-verified session", Turn::History("verified")),
        ("session with a resume frontier", Turn::History("reserved")),
        ("unreadable binding history", Turn::History("unreadable")),
        ("Herdr-hosted thread", Turn::Herdr),
        ("channel that is no reserved thread", Turn::NoThread),
    ];
    let root = crate::config::TestRuntimeRootGuard::new();
    p5::set_test_root(Some(root.path()));
    let shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
    for (index, (name, turn)) in cases.into_iter().enumerate() {
        let (fresh, id) = (matches!(turn, Turn::Reply), 8_372_200 + index as u64);
        let (mut provider, mut pane, mut history) = (Claude, Some(DeadOrAbsent), "");
        let (child, herdr) = (!matches!(turn, Turn::NoThread), matches!(turn, Turn::Herdr));
        match turn {
            Turn::By(other) => provider = other,
            Turn::Pane(other) => pane = other,
            Turn::History(other) => history = other,
            Turn::Reply | Turn::Herdr | Turn::NoThread => {}
        }
        let channel = ChannelId::new(id);
        let _policy = child.then(|| thread_child(id, PARENT));
        let hosts = || crate::config::session_hosts::force_for_test(Some("node"), &[(id, "node")]);
        let _hosts = herdr.then(hosts);
        let _pane = pane.map(|liveness| Pane::set(TMUX, liveness));
        let transcript = write_history(root.path(), id, history);
        assert_eq!(claude_thread::launch_policy(id).is_some(), child, "{name}");
        crate::services::discord::host_defer_gate::tests::map_channel(&shared, channel, "t").await;
        let stored = || Some(NATIVE.to_string());
        let mut core = shared.core.lock().await;
        core.sessions.get_mut(&channel).unwrap().session_id = stored();
        drop(core);
        let (mut session, mut memento, mut reason) = (stored(), true, "before");
        // The same person sends again, and the session row still names the session.
        for send in 0..2 {
            if send == 1 {
                session = stored();
            }
            let state = prep::Restore {
                session_id: &mut session,
                memento_context_loaded: &mut memento,
                reason: &mut reason,
                force_fresh: false,
                cleared: false,
                reset: false,
                recreate: false,
                session_key: None,
                tmux: pane.map(|_| TMUX),
            };
            prep::restore(&shared, &provider, channel, state).await;
        }
        let kept = (!fresh).then_some(NATIVE);
        let code = if fresh { REASON } else { "before" };
        assert_eq!(
            (session.as_deref(), reason, memento),
            (kept, code, !fresh),
            "{name}"
        );
        let cached = shared.core.lock().await.sessions[&channel]
            .session_id
            .clone();
        assert_eq!(cached.as_deref(), kept, "{name}");
        let profile = DispatchProfile::Full;
        let selected = session.as_deref();
        let none = None::<&()>;
        let recent = load_channel_recent_context(
            None, channel, selected, false, false, false, profile, None, none,
        );
        let gate = if fresh {
            "postgres_unavailable;pairs=0"
        } else {
            "resumed_session;pairs=0"
        };
        assert_eq!(recent.await.unwrap().audit_reason, gate, "{name}");
        if fresh {
            crate::services::discord::terminal_delivery_custody::install_dl_ports_for_tests();
            let cold = ResumePane::Cold;
            let refused = claude_thread::reserve_resume(id, TMUX, NATIVE, &transcript, cold);
            assert!(refused.unwrap_err().contains("has no verified binding"));
        }
        let notices = fresh_notice::posted_for_tests(NATIVE);
        assert_eq!(
            notices, 1,
            "one notice for the replaced session, none for {name}"
        );
    }
}
