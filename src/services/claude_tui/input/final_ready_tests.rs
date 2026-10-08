use super::{final_prompt_ready, prompt_readiness_snapshot_from_capture};

const RULE: &str = "────────────────────────────────────────────────────";
const STATUS: &str = "  🤖 Opus(H) │ 7% │ MCP: 2";

fn pane(above: &str, composer: &str) -> String {
    format!("{above}\n{RULE}\n{composer}\n{RULE}\n{STATUS}")
}

/// Only a live empty composer with no spinner, auth banner or draft is ready for an outside start.
#[test]
fn final_readiness_vetoes_spinner_auth_draft_blind_and_dead_panes() {
    let spinner = pane("✳ Architecting… (12s · esc to interrupt)", "❯ ");
    let auth = pane(" ⚠ 1 MCP server needs authentication · run /mcp", "❯ ");
    let draft = pane("✻ Baked for 2s", "❯ pending draft");
    let ready = pane("✻ Baked for 2s", "❯ ");
    let cases = [
        ("initial spinner", Some(spinner.as_str()), true),
        ("auth", Some(auth.as_str()), true),
        ("draft", Some(draft.as_str()), true),
        ("capture failed", None, true),
        ("pane dead", Some(ready.as_str()), false),
        ("empty composer", Some(ready.as_str()), true),
    ];
    let observed: Vec<_> = cases
        .iter()
        .map(|(name, capture, alive)| {
            let snapshot = prompt_readiness_snapshot_from_capture(*capture, *alive);
            (*name, final_prompt_ready(&snapshot))
        })
        .collect();
    assert_eq!(
        observed,
        [
            ("initial spinner", false),
            ("auth", false),
            ("draft", false),
            ("capture failed", false),
            ("pane dead", false),
            ("empty composer", true),
        ]
    );
}
