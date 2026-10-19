use super::*;
use crate::services::discord::recovery_engine::relay_output_replacements::relay_output_replacements;
use std::os::unix::fs::MetadataExt;

fn file_identity(path: &std::path::Path) -> (u64, u64) {
    let metadata = std::fs::metadata(path).unwrap();
    (metadata.dev(), metadata.ino())
}

/// A truncating prepare restarts the relay in a new file and announces it to the
/// idle scanner; an appending prepare keeps the same file and its bytes.
#[test]
fn truncating_prepare_restarts_the_relay_in_a_new_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("relay.jsonl");
    let path_text = path.to_str().unwrap();
    std::fs::write(&path, "old relay line\n").unwrap();
    let original = file_identity(&path);
    let replacements = relay_output_replacements(path_text);

    prepare(path_text, false).unwrap();
    assert_eq!(file_identity(&path), original, "append keeps the file");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "old relay line\n");
    assert_eq!(relay_output_replacements(path_text), replacements);

    prepare(path_text, true).unwrap();
    assert_ne!(
        file_identity(&path),
        original,
        "restarted bytes live in a new file"
    );
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
    assert_eq!(relay_output_replacements(path_text), replacements + 1);
    let entries = std::fs::read_dir(dir.path()).unwrap().count();
    assert_eq!(entries, 1, "no staging file is left behind");
}

#[test]
fn ensure_absent_reserves_once_without_advancing_a_healthy_generation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir
        .path()
        .join("resume.jsonl")
        .to_string_lossy()
        .into_owned();
    assert!(!present(&path));
    let (gate, generation) = ensure_absent(&path).unwrap();
    assert_eq!(generation, 1);
    assert!(ensure_absent(&path).is_none());
    assert_eq!(*gate.lock().unwrap_or_else(|p| p.into_inner()), generation);
    let (manual, next) = prepare(&path, false).unwrap();
    assert!(std::sync::Arc::ptr_eq(&gate, &manual));
    assert_eq!(next, generation + 1);
    drop((gate, manual));
    assert!(!present(&path));
    let held = std::sync::Arc::new(std::sync::Barrier::new(17));
    let release = std::sync::Arc::new(std::sync::Barrier::new(17));
    let threads: Vec<_> = (0..16)
        .map(|_| {
            let (path, held, release) = (path.clone(), held.clone(), release.clone());
            std::thread::spawn(move || {
                held.wait();
                let reservation = ensure_absent(&path);
                release.wait();
                reservation.is_some()
            })
        })
        .collect();
    held.wait();
    release.wait();
    assert_eq!(
        threads
            .into_iter()
            .map(|t| t.join().unwrap())
            .filter(|reserved| *reserved)
            .count(),
        1
    );
}
