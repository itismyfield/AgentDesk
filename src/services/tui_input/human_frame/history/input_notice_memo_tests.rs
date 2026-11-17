use crate::services::discord::{INPUT_NOTICE_RUNS, input_notice_content};
use crate::services::tui_input::human_frame::{history, idle};
use sha2::Digest;

/// A reused classification follows the history: a frame this process registers stops being a
/// card, and an outside same-length rewrite that restores the mtime makes it one again.
#[test]
fn classification_is_reused_only_under_the_same_history() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let card = "<subagent_notification>{\"status\":{\"completed\":\"x\"}}</subagent_notification>";
    let [prompt, other] = ["ann", "bob"].map(|name| idle(name, "1", &format!("\n{card}")));
    let registry = history::v2::Registry::runtime().unwrap();
    let plan = |id: u64, frame: &str| {
        let attempt = serde_json::json!({"nonce": format!("memo-{id}"), "source": {}});
        let input = serde_json::json!({"identity": {"sources": [id]}, "attempt": attempt,
            "original": {"author_id": 1}, "payload": {"frame": frame}});
        registry.prepare("memo", &input).unwrap();
    };
    let path = crate::config::runtime_root()
        .unwrap()
        .join("runtime/injected_frames.json");
    let at = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
    let settle = || {
        let file = std::fs::File::options().write(true).open(&path).unwrap();
        file.set_modified(at).unwrap();
    };
    let runs = || INPUT_NOTICE_RUNS.with(|runs| runs.replace(0));
    plan(1, "[User: carol (ID: 1)] unrelated");
    settle();
    assert_eq!(
        input_notice_content(&prompt),
        None,
        "an unknown frame is a card"
    );
    runs();
    assert_eq!(input_notice_content(&prompt), None);
    assert_eq!(
        runs(),
        0,
        "the same prompt under the same history is reused"
    );
    plan(2, &prompt);
    assert!(
        input_notice_content(&prompt).is_some(),
        "a registered frame is input"
    );
    settle();
    assert!(input_notice_content(&prompt).is_some());
    let raw = std::fs::read_to_string(&path).unwrap();
    let swap = |text: &str, from: &str, to: &str| {
        let hash = |frame: &str| format!("{:x}", sha2::Sha256::digest(frame.trim().as_bytes()));
        text.replace(&hash(from), &hash(to))
    };
    std::fs::write(&path, swap(&raw, &prompt, &other)).unwrap();
    settle();
    assert_eq!(
        input_notice_content(&prompt),
        None,
        "an outside rewrite is seen"
    );
}
