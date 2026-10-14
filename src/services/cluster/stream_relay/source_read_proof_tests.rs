use super::*;
use crate::services::tmux_common as tc;

#[cfg(unix)]
pub(crate) fn seal(session: &str, end: u64) -> (tempfile::TempDir, PrefixSeal) {
    seal_with_frontier(session, end, (7, 0))
}

#[cfg(unix)]
pub(crate) fn seal_with_frontier(
    session: &str,
    end: u64,
    frontier: (i64, u64),
) -> (tempfile::TempDir, PrefixSeal) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("source");
    let bytes = vec![b'x'; end as usize];
    std::fs::write(&path, &bytes).expect("source");
    let file = std::sync::Arc::new(std::fs::File::open(&path).expect("fd"));
    let pin = tc::with_tmux_source_authority(session, |authority| authority.pin()).expect("pin");
    let version = ReadVersion::from_fd(&file).expect("version");
    let proof = SourceReadProof::capture(file, path, pin, 0..end, &bytes, version, frontier)
        .expect("proof");
    (dir, proof.seal_prefix().expect("seal"))
}

#[test]
fn s2a1_source_revision_survives_weak_recreation_and_read_only_observation() {
    let _host = crate::config::pin_runtime_host_for_test();
    let session = "s2a1-weak-recreation";
    let key = tc::with_tmux_source_authority(session, |authority| authority.pin())
        .expect("initial pin")
        .key;
    let before = SourcePin::capture(key.clone()).expect("pin");
    tc::with_tmux_source_authority(session, |_| ());
    assert!(before.is_current());
    invalidate_source(&key);
    let after = SourcePin::capture(key).expect("new pin");
    assert_ne!(before, after);
    tc::with_tmux_source_authority(session, |_| ());
    assert!(after.is_current());
    assert!(!before.is_current());
}

#[test]
fn s2a1_overflow_is_sticky_and_never_changes_writer_result() {
    let _host = crate::config::pin_runtime_host_for_test();
    let session = "s2a1-revision-overflow";
    let key = tc::with_tmux_source_authority(session, |authority| authority.pin())
        .expect("initial pin")
        .key;
    REVISIONS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(key.clone(), Some(u64::MAX));
    let previous = SourcePin::capture(key.clone()).expect("max pin");
    invalidate_source(&key);
    assert!(!previous.is_current());
    assert!(SourcePin::capture(key.clone()).is_none());
    invalidate_source(&key);
    assert!(SourcePin::capture(key).is_none());
    tc::write_tmux_channel_binding(session, 59_960_103).expect("legacy writer still succeeds");
    assert_eq!(tc::read_tmux_channel_binding(session), Some(59_960_103));
    tc::cleanup_session_temp_files(session);
}

#[cfg(unix)]
#[test]
fn s2a1_original_fd_prefix_append_and_cursor_are_preserved() {
    let _host = crate::config::pin_runtime_host_for_test();
    use std::io::{Seek, Write};
    let (dir, seal) = seal("s2a1-fd-append", 100);
    let mut fd = seal.file.try_clone().expect("clone");
    fd.seek(std::io::SeekFrom::Start(17)).expect("cursor");
    let path = dir.path().join("source");
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("append fd")
        .write_all(b"tail")
        .expect("append");
    assert!(seal.is_current());
    assert_eq!(fd.stream_position().expect("position"), 17);
    std::fs::rename(&path, dir.path().join("old")).expect("rename");
    std::fs::write(&path, vec![b'y'; 100]).expect("replacement");
    assert!(
        !seal.is_current(),
        "the current path no longer names the retained fd"
    );
    std::fs::write(dir.path().join("old"), vec![b'z'; 104]).expect("rewrite original inode");
    assert!(!seal.is_current());
}

#[cfg(unix)]
#[test]
fn s2a1_read_versions_holes_and_restored_mtime_are_unmeasured() {
    let _host = crate::config::pin_runtime_host_for_test();
    let (dir, seal) = seal("s2a1-read-versions", 100);
    let file = seal.file.clone();
    let version = ReadVersion::from_fd(&file).expect("version");
    let proof = SourceReadProof::capture(
        file.clone(),
        seal.path.clone(),
        seal.pin.clone(),
        10..100,
        &[b'x'; 90],
        version,
        (7, 0),
    )
    .expect("range proof");
    assert!(proof.seal_prefix().is_none());
    assert!(
        SourceReadProof::capture(
            file.clone(),
            seal.path.clone(),
            seal.pin.clone(),
            0..101,
            &[b'x'; 100],
            version,
            (7, 0),
        )
        .is_none()
    );
    let modified = file
        .metadata()
        .expect("metadata")
        .modified()
        .expect("mtime");
    std::fs::write(dir.path().join("source"), [b'x'; 100]).expect("truncate regrow");
    let writer = std::fs::OpenOptions::new()
        .write(true)
        .open(dir.path().join("source"))
        .expect("writer");
    writer
        .set_times(std::fs::FileTimes::new().set_modified(modified))
        .expect("restore mtime");
    assert!(!seal.is_current());
    assert!(
        SourceReadProof::capture(
            file,
            seal.path,
            seal.pin,
            0..100,
            &[b'x'; 100],
            version,
            (7, 0)
        )
        .is_none()
    );
}

#[cfg(unix)]
#[test]
fn s2a1_codex_marker_only_mutation_invalidates_before_publication() {
    let _host = crate::config::pin_runtime_host_for_test();
    let _state = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let session = "s2a1-marker-entry";
    let dir = tempfile::tempdir().expect("dir");
    let first = dir.path().join("first.jsonl");
    let next = dir.path().join("next.jsonl");
    std::fs::write(&first, b"").expect("first");
    std::fs::write(&next, b"").expect("next");
    crate::services::codex_tui::session::write_codex_tui_rollout_marker(
        session,
        &first,
        Some("first"),
    )
    .expect("marker");
    let pin = tc::with_tmux_source_authority(session, |authority| authority.pin()).expect("pin");
    crate::services::codex_tui::session::write_codex_tui_rollout_marker(
        session,
        &next,
        Some("next"),
    )
    .expect("replace");
    assert!(!pin.is_current());
    let current =
        tc::with_tmux_source_authority(session, |authority| authority.pin()).expect("current");
    crate::services::codex_tui::session::write_codex_tui_rollout_marker_with_start_offset(
        session,
        &next,
        Some("next"),
        Some(20),
    )
    .expect("cursor only");
    assert!(current.is_current());
    tc::cleanup_session_temp_files(session);
}

#[cfg(unix)]
pub(crate) fn prefix(seal: &PrefixSeal, end: u64) -> PrefixSeal {
    let version = ReadVersion::from_fd(&seal.file).expect("version");
    SourceReadProof::capture(
        seal.file.clone(),
        seal.path.clone(),
        seal.pin.clone(),
        0..end,
        &vec![b'x'; end as usize],
        version,
        (seal.generation, seal.reset),
    )
    .expect("prefix proof")
    .seal_prefix()
    .expect("prefix seal")
}

#[cfg(unix)]
pub(crate) fn exhaust_revision(session: &str) {
    let key = tc::with_tmux_source_authority(session, |authority| authority.pin())
        .expect("pin")
        .key;
    REVISIONS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(key, None);
    assert!(tc::with_tmux_source_authority(session, |authority| authority.pin()).is_none());
}

pub(crate) fn hold_revisions_for_tests() -> impl Drop {
    REVISIONS.lock().unwrap_or_else(|error| error.into_inner())
}
