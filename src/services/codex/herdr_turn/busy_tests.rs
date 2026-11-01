//! The busy Codex Herdr composer is empty only as a dimmed placeholder on the colored capture;
//! any other screen, or a failed read before the owner's ready, sends no key and asks no ready.
use super::*;
use crate::db::dispatched_sessions::hosted_execution::{
    HostedExecution, HostedLocation, HostedOwner, HostedState, SourceRef,
};
use crate::services::herdr_admission::{Admission, force_for_test};
use crate::services::session_host::herdr_socket_rig_tests::{HerdrRig, KEY, NODE, PANE, SESSION};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::{Arc, Mutex};

/// The open model turn of the captured rollout's first eleven records.
const T: &str = "01a11d88-ef31-7860-9d48-88926c4b1809";
const PLACEHOLDER: &str = "\u{1b}[2mAsk Codex to do anything\u{1b}[0m";

fn fixture(name: &str) -> String {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex_busy_inject");
    std::fs::read_to_string(dir.join(name)).unwrap()
}

fn read(text: &str, truncated: bool) -> Value {
    json!({"type":"pane_read","read":{"pane_id":PANE,"workspace_id":"w1","tab_id":"w1:1",
        "source":"recent_unwrapped","format":"ansi","text":text,"revision":1,"truncated":truncated}})
}

struct Rig {
    rig: HerdrRig,
    target: HerdrTarget,
    rollout: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

fn rig() -> Rig {
    let rig = HerdrRig::start();
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    rig.run_provider(&rig.context(&nonce), false);
    let marker = crate::services::tmux_common::session_temp_path("busy-codex-herdr", "host_kind");
    std::fs::create_dir_all(Path::new(&marker).parent().unwrap()).unwrap();
    std::fs::write(marker, "herdr").unwrap();
    let owner = HostedOwner {
        provider: "codex".into(),
        discord_token_hash: "hash".into(),
        channel_id: "1".into(),
        logical_key: "busy-codex-herdr".into(),
        owner_node: NODE.into(),
        runtime_root: "test".into(),
    };
    let record = HostedExecution {
        schema: 1,
        state: HostedState::Bound,
        execution_nonce: nonce.clone(),
        owner,
        location: Some(HostedLocation {
            host: "herdr".into(),
            execution_node: NODE.into(),
            endpoint_config_key: KEY.into(),
            socket_addr: rig.socket().display().to_string(),
            named_session: SESSION.into(),
            pane_id: PANE.into(),
        }),
        expected: Some(rig.expected(&nonce)),
        source_ref: SourceRef {
            runtime_root: "test".into(),
            channel: "1".into(),
            provider: "codex".into(),
            logical_key: "busy-codex-herdr".into(),
            execution_nonce: nonce,
            initial_source: None,
            baseline_event_seq: None,
        },
    };
    let target = {
        let _registry = rig.registry_on_this_thread();
        crate::services::claude::herdr_turn::gate(&record).unwrap()
    };
    let dir = tempfile::tempdir().unwrap();
    let rollout = dir.path().join("rollout.jsonl");
    let records: String = fixture("model_turn.jsonl")
        .lines()
        .take(11)
        .map(|line| format!("{line}\n"))
        .collect();
    std::fs::write(&rollout, records).unwrap();
    Rig {
        rig,
        target,
        rollout,
        _dir: dir,
    }
}

/// Registers the nonce; `then` runs while it registers, as a person typing meanwhile would.
struct Typing<'a>(Option<Box<dyn Fn() + 'a>>);
impl crate::services::codex_tui::busy_inject::Ledger for Typing<'_> {
    fn register(&self) -> bool {
        if let Some(then) = &self.0 {
            then();
        }
        true
    }
    fn withdraw(&self) {}
}

/// One adapter attempt under an owner scope whose pin, asked by every ready, only counts itself.
fn attempt(rig: &Rig, case: usize, ledger: &Typing<'_>) -> (Outcome, usize) {
    let _registry = rig.rig.registry_on_this_thread();
    std::mem::forget(force_for_test(Admission::new(
        true,
        Some(std::env::temp_dir().join(format!("adk-bch-none-{}", uuid::Uuid::new_v4()))),
    )));
    let nonce = format!("{case:032x}");
    let request = Request {
        session: "busy-codex-herdr",
        rollout: &rig.rollout,
        source: "discord",
        author: "7",
        name: "tester",
        nonce: &nonce,
        text: "busy herdr question",
        target_turn: T,
    };
    let readies = Arc::new(Mutex::new(0));
    let seen = readies.clone();
    let outcome = crate::services::claude_tui::owner_input::with_scope(
        "codex".into(),
        json!({"host":"herdr","owner":"busy-codex-herdr","root":"/test","execution_nonce":"e"}),
        1,
        Arc::new(|_| Ok(())),
        Arc::new(move |_, _| {
            *seen.lock().unwrap() += 1;
            Ok(())
        }),
        Arc::new(|_| Ok(())),
        || inject(&rig.target, &request, ledger),
    );
    let readies = *readies.lock().unwrap();
    (outcome, readies)
}

#[test]
fn only_a_dimmed_placeholder_reads_as_an_empty_composer() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let empty = fixture("busy_empty.ansi");
    let with = |composer: &str| empty.replace(PLACEHOLDER, composer);
    let image = empty.replace(
        &format!("\n\n\u{1b}[1m›\u{1b}[0m {PLACEHOLDER}"),
        &format!("\n\n  [Image #1]\n\n\u{1b}[1m›\u{1b}[0m {PLACEHOLDER}"),
    );
    assert_ne!(image, empty);
    let cases = [
        (
            "an earlier empty composer above a draft",
            format!("{empty}\n{}", fixture("busy_draft.ansi")),
            false,
            Veto::ComposerUnread,
        ),
        (
            "a capture cut inside the composer",
            "  rest of a draft\n\n  gpt-5.4 xhigh · 87% context left\n".to_string(),
            false,
            Veto::ComposerUnread,
        ),
        (
            "a truncated read",
            empty.clone(),
            true,
            Veto::PaneUnavailable,
        ),
        (
            "a placeholder drawn plain",
            with("Ask Codex to do anything"),
            false,
            Veto::ComposerUnread,
        ),
        (
            "a partly dimmed row",
            with("\u{1b}[2mAsk Codex\u{1b}[0m to do anything"),
            false,
            Veto::ComposerUnread,
        ),
        (
            "a multiline draft",
            with("first row\n  second row"),
            false,
            Veto::ComposerUnread,
        ),
        (
            "an image above the composer",
            image,
            false,
            Veto::ComposerUnread,
        ),
        ("a modal", fixture("overlay.ansi"), false, Veto::Modal),
    ];
    for (case, (name, screen, truncated, veto)) in cases.into_iter().enumerate() {
        let rig = rig();
        rig.rig.answer("pane.read", read(&screen, truncated));
        let (outcome, readies) = attempt(&rig, case + 1, &Typing(None));
        assert_eq!(outcome, Outcome::NotSent(veto), "{name}");
        assert!(rig.rig.sends().is_empty(), "{name}: {:?}", rig.rig.sends());
        assert_eq!(readies, 0, "{name}");
    }
    let rig = rig();
    rig.rig.answer("pane.read", read(&empty, false));
    let (outcome, readies) = attempt(&rig, 100, &Typing(None));
    assert_eq!(outcome, Outcome::Injected);
    assert!(readies > 0, "the owner's ready runs before any key");
    let sends = rig.rig.sends();
    assert_eq!(sends.len(), 2, "{sends:?}");
    assert_eq!(sends[1]["keys"], json!(["enter"]));
}

#[test]
fn a_draft_typed_before_the_ready_withdraws_without_asking_it() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = rig();
    let empty = fixture("busy_empty.ansi");
    rig.rig.answer("pane.read", read(&empty, false));
    let typing = Typing(Some(Box::new(|| {
        rig.rig
            .answer("pane.read", read(&fixture("busy_draft.ansi"), false));
    })));
    let (outcome, readies) = attempt(&rig, 200, &typing);
    assert_eq!(outcome, Outcome::NotSent(Veto::ComposerUnread));
    assert_eq!(readies, 0, "a failed read never reaches the owner's ready");
    assert!(rig.rig.sends().is_empty(), "{:?}", rig.rig.sends());
}
