use super::*;
use std::io::{Read, Seek, SeekFrom};
type SourceChunk = Result<
    (
        Vec<u8>,
        u64,
        crate::services::cluster::stream_relay::SourceFileIdentity,
    ),
    String,
>;

pub(super) fn read_watcher_source_chunk(path: &str, offset: u64) -> SourceChunk {
    let file = std::fs::File::open(path).map_err(|error| format!("open: {error}"))?;
    read_watcher_source_chunk_from_file(file, offset)
}

fn read_watcher_source_chunk_from_file(mut file: std::fs::File, offset: u64) -> SourceChunk {
    let identity =
        crate::services::cluster::stream_relay::SourceFileIdentity::from_open_file(&file);
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
    mixed_read_provenance: bool,
) -> WatcherSourceAuthority {
    WatcherSourceAuthority {
        source_stamp: (!mixed_read_provenance)
            .then_some(authority.source_stamp)
            .flatten(),
        ..authority
    }
}

pub(super) fn source_authority_for_read(
    base: WatcherSourceAuthority,
    session_name: &str,
    marker: Option<crate::services::cluster::stream_relay::SourceWitness>,
    file: crate::services::cluster::stream_relay::SourceFileIdentity,
) -> WatcherSourceAuthority {
    WatcherSourceAuthority {
        generation_mtime_ns: base.generation_mtime_ns,
        reset_incarnation: base.reset_incarnation,
        source_stamp: marker.map(|marker| {
            crate::services::discord::delivery_lease_cell::source_epoch_observer::source_stamp(
                session_name,
                marker,
                file,
            )
        }),
    }
}

#[cfg(all(test, unix))]
#[path = "source_epoch_read_tests.rs"]
mod tests;
