//! The boot orphan file sweep leaves a protected channel's session files.

use super::sweep_orphan_session_files_in;
use crate::services::discord::input_runtime::fence::{Gate, HoldUnknownForTest, test_health};
use crate::services::provider::ProviderKind;

/// Writes an aged `<stem>.jsonl`, plus `<stem>.channel` naming `channel` when it is bound.
fn group(dir: &std::path::Path, stem: &str, channel: Option<u64>) -> std::path::PathBuf {
    let jsonl = dir.join(format!("{stem}.jsonl"));
    std::fs::write(&jsonl, b"{}\n").unwrap();
    let mut files = vec![jsonl.clone()];
    if let Some(channel) = channel {
        let binding = dir.join(format!("{stem}.channel"));
        std::fs::write(&binding, channel.to_string()).unwrap();
        files.push(binding);
    }
    let aged = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    for file in files {
        let file = std::fs::File::options().write(true).open(file).unwrap();
        file.set_modified(aged).unwrap();
    }
    jsonl
}

#[test]
fn c2b_orphan_file_sweep_leaves_protected_and_unplaced_groups() {
    let dir = tempfile::tempdir().unwrap();
    let (protected, legacy) = (6_325_575_001u64, 6_325_575_002u64);
    let gate = Gate::protect(ProviderKind::Codex, protected).unwrap();
    let _health = test_health::Clear::new(&gate);
    let protected_files = group(dir.path(), "agentdesk-c2b2a-h-protected", Some(protected));
    let legacy_files = group(dir.path(), "agentdesk-c2b2a-h-legacy", Some(legacy));
    let unplaced = group(dir.path(), "agentdesk-c2b2a-h-unplaced", None);
    let live = std::collections::HashSet::new();

    {
        let _hold = HoldUnknownForTest::new();
        sweep_orphan_session_files_in(dir.path(), &live);
    }
    assert!(protected_files.exists(), "a protected channel's files stay");
    assert!(!legacy_files.exists(), "an unprotected orphan is swept");
    assert!(
        unplaced.exists(),
        "an unplaced orphan holds while unknowns are held"
    );

    sweep_orphan_session_files_in(dir.path(), &live);
    assert!(protected_files.exists());
    assert!(
        !unplaced.exists(),
        "with nothing held an unplaced orphan is swept"
    );
}
