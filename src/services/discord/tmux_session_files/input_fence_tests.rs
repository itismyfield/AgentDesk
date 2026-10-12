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
    SCANS.with(|scans| scans.set(0));
    let dir = tempfile::tempdir().unwrap();
    let (protected, legacy) = (6_325_575_001u64, 6_325_575_002u64);
    let gate = Gate::protect(ProviderKind::Codex, protected).unwrap();
    let _health = test_health::Clear::new(&gate);
    let protected_files = group(dir.path(), "agentdesk-c2b2a-h-protected", Some(protected));
    let legacy_files = group(dir.path(), "agentdesk-c2b2a-h-legacy", Some(legacy));
    let unplaced = group(dir.path(), "agentdesk-c2b2a-h-unplaced", None);
    let live = std::collections::HashSet::new();
    let _hold = HoldUnknownForTest::new();
    sweep_orphan_session_files_in(dir.path(), &live);
    assert!(protected_files.exists(), "a protected channel's files stay");
    assert!(
        legacy_files.exists(),
        "global hold preserves a bound Legacy orphan"
    );
    assert_eq!(
        SCANS.with(std::cell::Cell::get),
        0,
        "helper holds before enumeration"
    );
    assert!(unplaced.exists(), "global hold preserves an unbound orphan");
}

#[cfg(unix)]
#[test]
fn n3bc_orphan_file_sweep_off_retains_main_gc() {
    use crate::services::discord::tmux::n4d2_entry_support as fixture;
    if !fixture::isolated(module_path!(), "n3bc_orphan_file_sweep_off_retains_main_gc") {
        return;
    }
    assert!(
        !crate::services::discord::input_runtime::fence::any_protected(),
        "off child has registry zero"
    );
    let dir = tempfile::tempdir().unwrap();
    let legacy_files = group(dir.path(), "agentdesk-n3bc-off-legacy", Some(6_325_575_003));
    let unplaced = group(dir.path(), "agentdesk-n3bc-off-unplaced", None);
    sweep_orphan_session_files_in(dir.path(), &std::collections::HashSet::new());
    assert!(
        !legacy_files.exists(),
        "off fixture still sweeps bound Legacy files"
    );
    assert!(!unplaced.exists(), "off fixture still sweeps unbound files");
}

thread_local! {
    static SCANS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
pub(super) fn scan_started() {
    SCANS.with(|scans| scans.set(scans.get() + 1));
}

#[cfg(unix)]
#[tokio::test]
async fn n3bc_public_file_sweep_preserves_six_protected_modes() {
    SCANS.with(|scans| scans.set(0));
    use crate::services::discord::input_runtime::fence::Mode;
    use crate::services::discord::tmux::n4d2_entry_support as fixture;
    if !fixture::isolated(
        module_path!(),
        "n3bc_public_file_sweep_preserves_six_protected_modes",
    ) {
        return;
    }
    let shared = crate::services::discord::make_shared_data_for_tests();
    let dir = crate::services::tmux_common::persistent_sessions_dir().unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    let modes = [
        Mode::LegacyOpen,
        Mode::Closing,
        Mode::Frozen,
        Mode::LedgerOpen,
        Mode::Handback,
        Mode::Held,
    ];
    for (n, mode) in modes.into_iter().enumerate() {
        let channel = 6_325_575_100 + n as u64;
        let gate = Gate::protect(ProviderKind::Claude, channel).unwrap();
        let _health = test_health::Clear::new(&gate);
        if mode != Mode::LegacyOpen {
            let closing = std::sync::Arc::new(gate.close().unwrap());
            if mode != Mode::Closing {
                let _transition = shared
                    .freeze_legacy_queue(
                        closing.clone(),
                        crate::services::turn_orchestrator::QueuePersistenceContext::new(
                            &ProviderKind::Claude,
                            "n3bc-files",
                            None,
                        ),
                    )
                    .await
                    .unwrap();
                match mode {
                    Mode::LedgerOpen => closing.open_ledger().unwrap(),
                    Mode::Handback => closing.begin_handback().unwrap(),
                    Mode::Held => closing.hold().unwrap(),
                    Mode::Frozen => {}
                    _ => unreachable!(),
                }
            }
        }
        assert_eq!(gate.mode(), mode);
        let files = [
            group(
                &dir,
                &format!("agentdesk-n3bc-public-{n}-bound"),
                Some(channel + 100),
            ),
            group(&dir, &format!("agentdesk-n3bc-public-{n}-unbound"), None),
        ];
        let before: Vec<_> = files
            .iter()
            .map(|file| {
                (
                    std::fs::read(file).unwrap(),
                    std::fs::metadata(file).unwrap().modified().unwrap(),
                )
            })
            .collect();
        let _hold = HoldUnknownForTest::new();
        super::sweep_orphan_session_files().await;
        assert_eq!(
            SCANS.with(std::cell::Cell::get),
            0,
            "public guard avoids enumeration in {mode:?}"
        );
        assert!(
            std::fs::read_to_string(fixture::root().join("tmux-calls"))
                .unwrap_or_default()
                .is_empty(),
            "public hold avoids live listing in {mode:?}"
        );
        for (file, (bytes, mtime)) in files.iter().zip(before) {
            assert!(file.exists(), "held global file survives in {mode:?}");
            assert_eq!(std::fs::read(file).unwrap(), bytes, "{mode:?}");
            assert_eq!(
                std::fs::metadata(file).unwrap().modified().unwrap(),
                mtime,
                "{mode:?}"
            );
        }
    }
}

#[test]
fn n3bc_file_sweep_rechecks_under_source_authority() {
    use crate::services::tmux_common::{
        SOURCE_AUTHORITY_CONTENDED, with_session_temp_source_authority,
    };
    let dir = tempfile::tempdir().unwrap();
    let stem = "agentdesk-n3bc-source-race";
    let file = group(dir.path(), stem, Some(6_325_575_302));
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let holding = std::thread::spawn(move || {
        with_session_temp_source_authority(stem, || {
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
    });
    entered_rx.recv().unwrap();
    let installed = std::rc::Rc::new(std::cell::RefCell::new(None));
    let keep = installed.clone();
    SOURCE_AUTHORITY_CONTENDED.with_borrow_mut(|hook| {
        *hook = Some(Box::new(move || {
            let gate = Gate::protect(ProviderKind::Codex, 6_325_575_301).unwrap();
            *keep.borrow_mut() = Some((test_health::Clear::new(&gate), HoldUnknownForTest::new()));
            release_tx.send(()).unwrap();
        }))
    });
    let before = std::fs::metadata(&file).unwrap().modified().unwrap();
    sweep_orphan_session_files_in(dir.path(), &std::collections::HashSet::new());
    holding.join().unwrap();
    assert!(
        installed.borrow().is_some(),
        "protection installed after authority lock wait"
    );
    assert!(file.exists(), "source-authority recheck preserves file");
    assert_eq!(std::fs::read(&file).unwrap(), b"{}\n");
    assert_eq!(
        std::fs::metadata(&file).unwrap().modified().unwrap(),
        before
    );
}
