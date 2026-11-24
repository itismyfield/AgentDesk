//! The session a thread turn selects, read where the intake prepares it.

use super::REASON;
use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
use crate::services::claude::{fresh_notice, tui_session_launch::ResumePane};
use crate::services::discord::prompt_builder::{DispatchProfile, load_channel_recent_context};
use crate::services::discord::router::message_handler::intake_turn::session_preparation::{
    Restore, restore,
};
use crate::services::platform::tmux::PaneLiveness::{self, DeadOrAbsent, Live};
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

/// `thread` as boot leaves a thread found under a late-adopt Claude TUI `parent`.
pub(in crate::services::discord) fn thread_child(
    thread: u64,
    parent: u64,
) -> test_override::ChannelsGuard {
    let config: crate::config::Config = serde_json::from_value(json!({
        "server": {},
        "agents": [{"id": "c", "name": "C",
            "channels": {"claude": {"id": parent.to_string(), "runtime": "tui"}}}],
        "tui_o": {"writer": {"channels": [parent], "late_adopt": true},
            "turn": {"channels": [parent]}}
    }))
    .unwrap();
    let pending = |_: &_, _| Ok(std::collections::BTreeMap::from([(parent, Adoption::Pending)]));
    let boot = BootChannels::validate(&config).unwrap();
    let boot = boot.seeded(&config, pending).unwrap();
    let surface = serde_json::from_value(json!({
        "id": thread.to_string(), "parent_id": parent.to_string(), "type": 11, "name": "t"
    }))
    .unwrap();
    boot.reserve_boot_children(&[(thread, parent)]);
    let surface = VerifiedSurface::from_discord(&surface, ClaudeTui).unwrap();
    assert!(boot.resolve_child(thread, surface, false));
    test_override::force_boot(boot)
}

/// What the thread's durable files say about `NATIVE` before the turn.
#[derive(Clone, Copy, Debug)]
enum History {
    Unbound,
    Verified,
    Reserved,
    Unreadable,
}

fn write_history(root: &std::path::Path, channel: u64, parent: u64, history: History) {
    let transcript = root.join(format!("{channel}.jsonl"));
    std::fs::write(&transcript, "{}\n").unwrap();
    let source = source_id_for(NATIVE, &transcript).unwrap();
    let line = match history {
        History::Unbound => return,
        History::Reserved => {
            let resume = json!([{"parent": parent, "tmux": TMUX, "source": source,
                "offset": 0, "sha256": ""}]);
            let dir = root.join("runtime/claude_thread_resumes");
            std::fs::create_dir_all(&dir).unwrap();
            return std::fs::write(dir.join(format!("{channel}.json")), resume.to_string())
                .unwrap();
        }
        History::Unreadable => "not a binding record".to_string(),
        History::Verified => {
            let at = chrono::Utc::now();
            json!({"seq": 1, "channel_id": channel, "provider": "claude", "tmux_session": TMUX,
                "execution_nonce": "n", "old": null, "new": {"source": source},
                "cause": "same_session_verified", "parent_hint": null, "committed_at": at,
                "evidence": {"hook_event": "UserPromptSubmit", "received_at": at},
                "verified": true})
            .to_string()
        }
    };
    let dir = root.join(p5::BINDING_EVENTS_DIR);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{channel}.log")), line + "\n").unwrap();
}

// Only a Claude turn headed for a cold pane in a reserved thread, whose stored session nothing
// verified, is started fresh; it then carries recent messages and is no longer a refused resume.
#[tokio::test]
async fn unverified_thread_cold_resume_starts_fresh() {
    const PARENT: u64 = 8_372_100;
    use History::*;
    let cases: [(
        &str,
        ProviderKind,
        Option<PaneLiveness>,
        History,
        bool,
        bool,
        bool,
    ); 9] = [
        (
            "process session left by a routine",
            Claude,
            Some(DeadOrAbsent),
            Unbound,
            true,
            false,
            true,
        ),
        (
            "input joining a live pane",
            Claude,
            Some(Live),
            Unbound,
            true,
            false,
            false,
        ),
        (
            "pane that could not be read",
            Claude,
            Some(PaneLiveness::ProbeError),
            Unbound,
            true,
            false,
            false,
        ),
        (
            "Codex turn in a Claude thread",
            Codex,
            Some(DeadOrAbsent),
            Unbound,
            true,
            false,
            false,
        ),
        (
            "hook-verified session",
            Claude,
            Some(DeadOrAbsent),
            Verified,
            true,
            false,
            false,
        ),
        (
            "session with a resume frontier",
            Claude,
            Some(DeadOrAbsent),
            Reserved,
            true,
            false,
            false,
        ),
        (
            "unreadable binding history",
            Claude,
            Some(DeadOrAbsent),
            Unreadable,
            true,
            false,
            false,
        ),
        (
            "turn without a pane",
            Claude,
            None,
            Unbound,
            true,
            false,
            false,
        ),
        (
            "channel that is no reserved thread",
            Claude,
            Some(DeadOrAbsent),
            Unbound,
            false,
            false,
            false,
        ),
    ];
    let herdr = (
        "Herdr-hosted thread",
        Claude,
        Some(DeadOrAbsent),
        Unbound,
        true,
        true,
        false,
    );
    let root = crate::config::TestRuntimeRootGuard::new();
    p5::set_test_root(Some(root.path()));
    let shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
    for (index, (name, provider, pane, history, child, herdr, fresh)) in
        cases.into_iter().chain([herdr]).enumerate()
    {
        let id = 8_372_200 + index as u64;
        let channel = ChannelId::new(id);
        let _policy = child.then(|| thread_child(id, PARENT));
        let _hosts = herdr
            .then(|| crate::config::session_hosts::force_for_test(Some("node"), &[(id, "node")]));
        let _pane = pane.map(|liveness| Pane::set(TMUX, liveness));
        write_history(root.path(), id, PARENT, history);
        assert_eq!(claude_thread::launch_policy(id).is_some(), child, "{name}");
        crate::services::discord::host_defer_gate::tests::map_channel(&shared, channel, "t").await;
        shared
            .core
            .lock()
            .await
            .sessions
            .get_mut(&channel)
            .unwrap()
            .session_id = Some(NATIVE.into());
        let (mut session, mut memento, mut reason) = (Some(NATIVE.to_string()), true, "before");
        for _again in 0..2 {
            let state = Restore {
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
            restore(&shared, &provider, channel, state).await;
        }
        let kept = (!fresh).then_some(NATIVE);
        assert_eq!(
            (session.as_deref(), reason),
            (kept, if fresh { REASON } else { "before" }),
            "{name}"
        );
        let cached = shared.core.lock().await.sessions[&channel]
            .session_id
            .clone();
        assert_eq!((cached.as_deref(), memento), (kept, !fresh), "{name}");
        let recent = load_channel_recent_context(
            None,
            channel,
            session.as_deref(),
            false,
            false,
            false,
            DispatchProfile::Full,
            None,
            None::<&()>,
        );
        let gate = if fresh {
            "postgres_unavailable;pairs=0"
        } else {
            "resumed_session;pairs=0"
        };
        assert_eq!(recent.await.unwrap().audit_reason, gate, "{name}");
        if fresh {
            let launch = |native: Option<&str>| {
                let path = root.path().join(format!("{id}.jsonl"));
                native.map_or(Ok(()), |native| {
                    claude_thread::reserve_resume(id, TMUX, native, &path, ResumePane::Cold)
                })
            };
            let refused = launch(Some(NATIVE)).unwrap_err();
            assert!(refused.contains("has no verified binding"), "{refused}");
            assert_eq!(launch(session.as_deref()), Ok(()));
            assert_eq!(
                fresh_notice::posted_for_tests(NATIVE),
                1,
                "one notice per session"
            );
        }
    }
    assert_eq!(
        fresh_notice::posted_for_tests(NATIVE),
        1,
        "kept sessions post no notice"
    );
}
