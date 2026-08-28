use super::*;
use crate::services::cluster::stream_relay::{
    GenerationSourceIdentity, SourceFileIdentity, SourceWitness,
};
#[test]
fn same_fd_identity_mode_resample_and_mixed_utf8_policy() {
    let session = format!("watcher-source-{}", uuid::Uuid::new_v4().simple());
    let base = WatcherSourceAuthority {
        generation_mtime_ns: 77,
        reset_incarnation: 9,
        source_stamp: None,
    };
    let marker = SourceWitness {
        generation: Some(GenerationSourceIdentity::Unix {
            mtime_ns: 88,
            dev: 1,
            ino: 2,
        }),
        spawn_nonce_hash: Some([3; 32]),
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("source.jsonl");
    let replacement_path = dir.path().join("replacement.jsonl");
    std::fs::write(&path, b"old-bytes").unwrap();
    let old_file = std::fs::File::open(&path).unwrap();
    let old_metadata = old_file.metadata().unwrap();
    std::fs::write(&replacement_path, b"new-bytes").unwrap();
    std::fs::rename(&replacement_path, &path).unwrap();
    let (old_bytes, _, old_identity) = read_watcher_source_chunk_from_file(old_file, 0).unwrap();
    assert_eq!(old_bytes, b"old-bytes");
    assert_eq!(
        old_identity,
        SourceFileIdentity::Unix {
            dev: std::os::unix::fs::MetadataExt::dev(&old_metadata),
            ino: std::os::unix::fs::MetadataExt::ino(&old_metadata),
        }
    );
    let (replacement_bytes, _, replacement_identity) =
        read_watcher_source_chunk(path.to_str().unwrap(), 0).unwrap();
    assert_eq!(replacement_bytes, b"new-bytes");
    assert_ne!(replacement_identity, old_identity);
    let observed = source_authority_for_read(base, &session, Some(marker), old_identity);
    let first = observed.source_stamp;
    assert_eq!(first.unwrap().file, old_identity);
    let legacy = source_authority_for_read(observed, &session, None, replacement_identity);
    assert_eq!(
        (
            legacy.source_stamp,
            legacy.generation_mtime_ns,
            legacy.reset_incarnation
        ),
        (None, 77, 9)
    );
    let replacement =
        source_authority_for_read(legacy, &session, Some(marker), replacement_identity);
    let different = replacement.source_stamp;
    assert_eq!(different.unwrap().file, replacement_identity);
    assert_eq!(merged_source_stamp(Some(first), first), first);
    assert_eq!(merged_source_stamp(Some(first), different), None);
    assert_eq!(merged_source_stamp(Some(first), None), None);
    assert_eq!(merged_source_stamp(Some(None), first), None);
    let mixed = authority_for_decoded_text(replacement, true);
    assert_eq!(mixed.source_stamp, None);
    assert_eq!(
        (mixed.generation_mtime_ns, mixed.reset_incarnation),
        (77, 9)
    );
    let collector = include_str!("turn_stream_collector.rs");
    assert!(collector.contains("unwrap_or(Some(None))"));
    assert!(collector.contains("if !restored_response_seed.is_empty()"));
    let idle = include_str!("../session_relay_sink.rs");
    let independent = "let source_marker = source_epoch_observer::marker_if_enabled(&session_name);\n            let current_generation_signature =\n                super::tmux::read_generation_file_mtime_ns(&session_name);";
    assert!(idle.contains(independent));
    assert!(!idle.contains("marker_and_generation_signature"));
    assert!(!include_str!("../session_relay_sink/turn_parser.rs").contains("relay_source_stamp"));
    assert!(collector.contains("source_stamp: aggregate_source_stamp.flatten()"));
}
