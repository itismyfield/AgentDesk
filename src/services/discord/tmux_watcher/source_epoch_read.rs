use super::*;
use crate::services::cluster::stream_relay::{SourceFileIdentity, SourceStamp, SourceWitness};
use std::io::{Read, Seek, SeekFrom};
type SourceChunk = Result<(Vec<u8>, u64, SourceFileIdentity), String>;

pub(super) fn read_watcher_source_chunk(path: &str, offset: u64) -> SourceChunk {
    read_watcher_source_chunk_from_file(
        std::fs::File::open(path).map_err(|error| format!("open: {error}"))?,
        offset,
    )
}

fn read_watcher_source_chunk_from_file(mut file: std::fs::File, offset: u64) -> SourceChunk {
    let identity = SourceFileIdentity::from_open_file(&file);
    file.seek(SeekFrom::Start(offset))
        .map_err(|error| format!("seek: {error}"))?;
    let mut bytes = vec![0_u8; 16_384];
    let read = file
        .read(&mut bytes)
        .map_err(|error| format!("read: {error}"))?;
    bytes.truncate(read);
    Ok((bytes, offset + read as u64, identity))
}

pub(super) fn authority_for_decoded_text(
    authority: WatcherSourceAuthority,
    mixed: bool,
) -> WatcherSourceAuthority {
    WatcherSourceAuthority {
        source_stamp: (!mixed).then_some(authority.source_stamp).flatten(),
        ..authority
    }
}

pub(super) fn merged_source_stamp(
    aggregate: Option<Option<SourceStamp>>,
    contribution: Option<SourceStamp>,
) -> Option<SourceStamp> {
    match (aggregate, contribution) {
        (Some(Some(left)), Some(right)) if left == right => Some(left),
        (None, contribution) => contribution,
        _ => None,
    }
}

pub(super) fn source_authority_for_read(
    base: WatcherSourceAuthority,
    session: &str,
    witness: Option<SourceWitness>,
    file: SourceFileIdentity,
) -> WatcherSourceAuthority {
    WatcherSourceAuthority {
        source_stamp: witness.and_then(|witness| {
            crate::services::discord::delivery_lease_cell::source_epoch_observer::source_stamp(
                session, witness, file,
            )
        }),
        ..base
    }
}
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::services::cluster::stream_relay::GenerationSourceIdentity;
    #[test]
    #[rustfmt::skip]
    fn same_fd_identity_mode_resample_and_mixed_utf8_policy() {
        let session = format!("watcher-source-{}", uuid::Uuid::new_v4().simple()); let base = WatcherSourceAuthority { generation_mtime_ns: 77, reset_incarnation: 9, source_stamp: None };
        let witness = SourceWitness { generation: Some(GenerationSourceIdentity::Unix { mtime_ns: 88, dev: 1, ino: 2 }), spawn_nonce_hash: Some([3; 32]) };
        let dir = tempfile::tempdir().unwrap(); let path = dir.path().join("source.jsonl"); let replacement = dir.path().join("replacement.jsonl");
        std::fs::write(&path, b"old-bytes").unwrap(); let old_file = std::fs::File::open(&path).unwrap();
        std::fs::write(&replacement, b"new-bytes").unwrap(); std::fs::rename(&replacement, &path).unwrap();
        let (old_bytes, _, old_id) = read_watcher_source_chunk_from_file(old_file, 0).unwrap();
        let (new_bytes, _, new_id) = read_watcher_source_chunk(path.to_str().unwrap(), 0).unwrap();
        assert_eq!((old_bytes.as_slice(), new_bytes.as_slice()), (b"old-bytes".as_slice(), b"new-bytes".as_slice())); assert_ne!(old_id, new_id);
        let first = source_authority_for_read(base, &session, Some(witness), old_id); let known = first.source_stamp;
        let legacy = source_authority_for_read(first, &session, None, new_id); assert_eq!((legacy.source_stamp, legacy.generation_mtime_ns, legacy.reset_incarnation), (None, 77, 9));
        let next = source_authority_for_read(legacy, &session, Some(witness), new_id); assert_ne!(known, next.source_stamp);
        assert_eq!((merged_source_stamp(Some(known), known), merged_source_stamp(Some(known), next.source_stamp), merged_source_stamp(Some(known), None), merged_source_stamp(Some(None), known)), (known, None, None, None));
        let mixed = authority_for_decoded_text(next, true); assert_eq!((mixed.source_stamp, mixed.generation_mtime_ns, mixed.reset_incarnation), (None, 77, 9));
        let mut decoder = Utf8ChunkDecoder::default(); let bytes = "안".as_bytes(); assert!(!decoder.decode(&bytes[..1], 0).mixed_read_provenance); assert!(decoder.decode(&bytes[1..], 1).mixed_read_provenance);
    }
}
