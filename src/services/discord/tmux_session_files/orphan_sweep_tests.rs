//! The boot orphan file sweep removes aged session file groups that no live tmux names.

use super::sweep_orphan_session_files_in;

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
fn orphan_file_sweep_removes_aged_bound_and_unbound_groups() {
    let dir = tempfile::tempdir().unwrap();
    let legacy_files = group(dir.path(), "agentdesk-n3bc-off-legacy", Some(6_325_575_003));
    let unplaced = group(dir.path(), "agentdesk-n3bc-off-unplaced", None);
    sweep_orphan_session_files_in(dir.path(), &std::collections::HashSet::new());
    assert!(
        !legacy_files.exists(),
        "the sweep removes bound Legacy files"
    );
    assert!(
        !dir.path()
            .join("agentdesk-n3bc-off-legacy.channel")
            .exists(),
        "the sweep removes the whole bound group"
    );
    assert!(!unplaced.exists(), "the sweep removes unbound files");
}
