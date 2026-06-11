//! Detection and dismissal policy for Claude Code TUI startup dialogs.
//!
//! Claude Code 2.1.170 can interpose modal dialogs between TUI launch and the
//! first usable composer prompt: a "Resume from summary" picker when resuming
//! a large/old session, and a workspace-trust confirmation when the spawn cwd
//! is not yet trusted. Both render an option selector whose highlighted row
//! (`❯ 1. ...`) reads as a composer draft to the prompt-readiness scrape, so
//! without explicit handling readiness blocks until the full timeout and the
//! turn fails with `reason=prompt_marker_not_detected`.

use std::path::Path;

/// Footer shared by Claude Code modal dialogs. Selector overlays such as
/// `/effort` render the same footer, so detection always pairs it with a
/// dialog-specific marker line and never fires on the footer alone.
const DIALOG_FOOTER_MARKER: &str = "Enter to confirm";
const RESUME_FROM_SUMMARY_MARKER: &str = "Resume from summary";
const WORKSPACE_TRUST_MARKER: &str = "Quick safety check";
const WORKSPACE_TRUST_PATH_HEADER: &str = "Accessing workspace:";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ClaudeStartupDialog {
    /// Large/old-session resume picker. Option 1 ("Resume from summary",
    /// recommended) is pre-highlighted, so a bare Enter accepts it.
    ResumeFromSummary,
    /// Workspace trust confirmation. Option 1 ("Yes, I trust this folder") is
    /// pre-highlighted. `workspace` is the path the dialog displays; empty
    /// when the path line could not be located in the capture.
    WorkspaceTrust { workspace: String },
}

impl ClaudeStartupDialog {
    pub(crate) fn label(&self) -> &'static str {
        match self {
            ClaudeStartupDialog::ResumeFromSummary => "resume-from-summary",
            ClaudeStartupDialog::WorkspaceTrust { .. } => "workspace-trust",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum StartupDialogPlan {
    /// Press Enter to accept the pre-highlighted recommended option.
    DismissWithEnter,
    /// Refuse to auto-trust the displayed workspace and fail readiness fast
    /// with an operator-actionable error instead of burning the full timeout.
    FailUntrustedWorkspace { workspace: String },
}

pub(crate) fn detect_claude_startup_dialog(pane_tail: &str) -> Option<ClaudeStartupDialog> {
    if !pane_tail.contains(DIALOG_FOOTER_MARKER) {
        return None;
    }
    if pane_tail.contains(RESUME_FROM_SUMMARY_MARKER) {
        return Some(ClaudeStartupDialog::ResumeFromSummary);
    }
    if pane_tail.contains(WORKSPACE_TRUST_MARKER) {
        let workspace = workspace_trust_dialog_path(pane_tail).unwrap_or_default();
        return Some(ClaudeStartupDialog::WorkspaceTrust { workspace });
    }
    None
}

pub(crate) fn plan_startup_dialog_response(dialog: &ClaudeStartupDialog) -> StartupDialogPlan {
    plan_startup_dialog_response_with_home(dialog, dirs::home_dir().as_deref())
}

fn plan_startup_dialog_response_with_home(
    dialog: &ClaudeStartupDialog,
    home: Option<&Path>,
) -> StartupDialogPlan {
    match dialog {
        ClaudeStartupDialog::ResumeFromSummary => StartupDialogPlan::DismissWithEnter,
        ClaudeStartupDialog::WorkspaceTrust { workspace } => {
            if workspace_trust_auto_accept_allowed(workspace, home) {
                StartupDialogPlan::DismissWithEnter
            } else {
                StartupDialogPlan::FailUntrustedWorkspace {
                    workspace: workspace.clone(),
                }
            }
        }
    }
}

/// Auto-trust is restricted to paths inside the operator's home directory:
/// AgentDesk only spawns Claude TUI sessions in workspaces it manages there,
/// so a trust dialog for such a path is just first-contact friction. Anything
/// else — the observed case being `/` from a missing channel→workspace
/// mapping — is a spawn-config bug that must surface as an error, not get
/// rubber-stamped into a root-trusted Claude.
fn workspace_trust_auto_accept_allowed(workspace: &str, home: Option<&Path>) -> bool {
    let Some(home) = home else {
        return false;
    };
    if workspace.is_empty() {
        return false;
    }
    let workspace = Path::new(workspace);
    workspace.is_absolute() && workspace.starts_with(home)
}

/// The trust dialog renders the workspace path on its own line below the
/// `Accessing workspace:` header (blank line in between). A long path may
/// wrap, in which case the first line is a prefix of the real path — still
/// sufficient for the under-home policy check, since a path prefix inside the
/// home directory implies the full path is too.
fn workspace_trust_dialog_path(pane_tail: &str) -> Option<String> {
    let mut after_header = false;
    for line in pane_tail.lines() {
        let trimmed = line.trim();
        if after_header && !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
        if trimmed.contains(WORKSPACE_TRUST_PATH_HEADER) {
            after_header = true;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // Captured verbatim from ~/.adk/release/debug/claude_tui.log on
    // 2026-06-11 (AgentDesk-claude----agentdesk-claude, readiness=fresh).
    const RESUME_DIALOG_PANE: &str = "\
────────────────────────────────────────────────────────────────────────────────
  This session is 10h 50m old and 367.3k tokens.

  Resuming the full session will consume a substantial portion of your usage
  limits. We recommend resuming from a summary.

  ❯ 1. Resume from summary (recommended)
    2. Resume full session as-is
    3. Don't ask me again

  Enter to confirm · Esc to cancel";

    // Captured verbatim from ~/.adk/release/debug/claude_tui.log on
    // 2026-06-10 (AgentDesk-claude---agent-game-play spawned with cwd `/`).
    const TRUST_DIALOG_ROOT_PANE: &str = "\
────────────────────────────────────────────────────────────────────────────────
 Accessing workspace:

 /

 Quick safety check: Is this a project you created or one you trust? (Like your
 own code, a well-known open source project, or work from your team). If not,
 take a moment to review what's in this folder first.

 Claude Code'll be able to read, edit, and execute files here.

 Security guide

 ❯ 1. Yes, I trust this folder
   2. No, exit

 Enter to confirm · Esc to cancel";

    fn trust_dialog_pane_for(path: &str) -> String {
        TRUST_DIALOG_ROOT_PANE.replace("\n /\n", &format!("\n {path}\n"))
    }

    #[test]
    fn resume_from_summary_dialog_is_detected_and_dismissed_with_enter() {
        let dialog = detect_claude_startup_dialog(RESUME_DIALOG_PANE)
            .expect("resume dialog must be detected");
        assert_eq!(dialog, ClaudeStartupDialog::ResumeFromSummary);
        assert_eq!(
            plan_startup_dialog_response_with_home(&dialog, Some(Path::new("/Users/kunkun"))),
            StartupDialogPlan::DismissWithEnter
        );
    }

    #[test]
    fn trust_dialog_for_root_workspace_fails_fast_instead_of_auto_trusting() {
        let dialog = detect_claude_startup_dialog(TRUST_DIALOG_ROOT_PANE)
            .expect("trust dialog must be detected");
        assert_eq!(
            dialog,
            ClaudeStartupDialog::WorkspaceTrust {
                workspace: "/".to_string()
            }
        );
        assert_eq!(
            plan_startup_dialog_response_with_home(&dialog, Some(Path::new("/Users/kunkun"))),
            StartupDialogPlan::FailUntrustedWorkspace {
                workspace: "/".to_string()
            }
        );
    }

    #[test]
    fn trust_dialog_for_home_workspace_is_auto_accepted() {
        let pane = trust_dialog_pane_for("/Users/kunkun/.adk/release/workspaces/gamer");
        let dialog = detect_claude_startup_dialog(&pane).expect("trust dialog must be detected");
        assert_eq!(
            plan_startup_dialog_response_with_home(&dialog, Some(Path::new("/Users/kunkun"))),
            StartupDialogPlan::DismissWithEnter
        );
    }

    #[test]
    fn trust_dialog_outside_home_is_rejected_component_wise() {
        // `/Users/kunkun2` must not pass a `/Users/kunkun` home check; the
        // policy compares path components, not string prefixes.
        let pane = trust_dialog_pane_for("/Users/kunkun2/workspace");
        let dialog = detect_claude_startup_dialog(&pane).expect("trust dialog must be detected");
        assert_eq!(
            plan_startup_dialog_response_with_home(&dialog, Some(Path::new("/Users/kunkun"))),
            StartupDialogPlan::FailUntrustedWorkspace {
                workspace: "/Users/kunkun2/workspace".to_string()
            }
        );
    }

    #[test]
    fn trust_dialog_with_unparseable_path_is_rejected() {
        let pane = TRUST_DIALOG_ROOT_PANE.replace("Accessing workspace:", "");
        let dialog = detect_claude_startup_dialog(&pane).expect("trust dialog must be detected");
        assert_eq!(
            dialog,
            ClaudeStartupDialog::WorkspaceTrust {
                workspace: String::new()
            }
        );
        assert!(matches!(
            plan_startup_dialog_response_with_home(&dialog, Some(Path::new("/Users/kunkun"))),
            StartupDialogPlan::FailUntrustedWorkspace { .. }
        ));
    }

    #[test]
    fn effort_selector_footer_alone_is_not_a_startup_dialog() {
        // The `/effort` slider shares the `Enter to confirm` footer; it must
        // not be mistaken for a startup dialog and dismissed.
        let pane = "\
  Select effort level

  low ── medium ── high

  ←/→ to adjust · Enter to confirm · Esc to cancel";
        assert_eq!(detect_claude_startup_dialog(pane), None);
    }

    #[test]
    fn busy_compacting_pane_is_not_a_startup_dialog() {
        let pane = "\
❯ /compact

· Compacting conversation… (30s)
────────────────────────────────────────────────────────────────────────────────
  ⏵⏵ bypass permissions on (shift+tab to cycle) · esc to interrupt";
        assert_eq!(detect_claude_startup_dialog(pane), None);
    }

    #[test]
    fn idle_ready_pane_is_not_a_startup_dialog() {
        let pane = "\
────────────────────────────────────────────────────────────────────────────────
❯
────────────────────────────────────────────────────────────────────────────────
  ⏵⏵ bypass permissions on (shift+tab to cycle)";
        assert_eq!(detect_claude_startup_dialog(pane), None);
    }

    #[test]
    fn auto_accept_requires_home_dir_resolution() {
        assert!(!workspace_trust_auto_accept_allowed(
            "/Users/kunkun/project",
            None
        ));
    }
}
