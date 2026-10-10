//! Which pieces a re-post may cover, and the identity every node derives for one.

use sha2::{Digest, Sha256};

pub(crate) use super::o_piece_delivery::PieceKey;
use super::o_piece_delivery::{kind_name, provider_name};
use crate::services::tui_o::store::ledger::PieceRecord;

/// Bumped when the key fields change meaning; a stored row of another version is a conflict.
pub(crate) const IDENTITY_VERSION: i32 = 1;
/// Bumped when a body splits differently; a stored piece is re-sent as stored, never re-split.
pub(crate) const SPLIT_VERSION: i32 = 1;

pub(crate) fn payload_sha256(payload: &str) -> String {
    hex::encode(Sha256::digest(payload.as_bytes()))
}

/// The piece a ledger record prepared, when a re-post may cover it.
pub(crate) fn piece_of(record: &PieceRecord) -> Option<PieceKey> {
    if record.payload.trim().is_empty() {
        return None;
    }
    PieceKey::new(record.unit_key.clone(), record.piece_index)
}

/// The marker every send of the piece carries outside its content, the same on every node.
pub(crate) fn marker(key: &PieceKey) -> String {
    let unit = key.unit();
    let (provider, kind) = (provider_name(unit.provider), kind_name(unit.kind));
    format!(
        "o:{provider}:{kind}:{}#{}",
        unit.native_key,
        key.piece_index()
    )
}
