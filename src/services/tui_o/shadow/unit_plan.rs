//! Splits a unit's payload exactly as Legacy does before posting and digests the pieces.

use sha2::{Digest, Sha256};

use super::PieceDigest;
use super::identity::UnitContent;
use crate::services::discord::DISCORD_MSG_LIMIT;
use crate::services::discord::formatting::split_for_shadow;

/// A planned unit: the pieces O would post, or why it posts nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UnitPlan {
    Pieces(Vec<PieceDigest>),
    Excluded(&'static str),
}

/// Plans one unit; `Err` carries the reason the unit must stay schema-blocked.
pub fn plan(content: &UnitContent) -> Result<UnitPlan, String> {
    let payload = match content {
        UnitContent::Excluded(reason) => return Ok(UnitPlan::Excluded(reason)),
        // Trimmed the way the writer codec records a payload.
        UnitContent::Payload(text) => text.trim(),
    };
    if payload.is_empty() {
        return Ok(UnitPlan::Excluded("empty_payload"));
    }
    digest_pieces(split_for_shadow(payload)).map(UnitPlan::Pieces)
}

/// Digests split pieces; a piece over the Discord unit limit blocks the whole unit.
pub fn digest_pieces(pieces: Vec<(String, usize)>) -> Result<Vec<PieceDigest>, String> {
    let digest = |(index, (piece, units)): (usize, (String, usize))| {
        if units > DISCORD_MSG_LIMIT {
            return Err(format!("split piece {index} has {units} UTF-16 units"));
        }
        Ok(PieceDigest {
            index: u32::try_from(index).unwrap_or(u32::MAX),
            units: u32::try_from(units).unwrap_or(u32::MAX),
            sha256: hex::encode(Sha256::digest(piece.as_bytes())),
        })
    };
    pieces.into_iter().enumerate().map(digest).collect()
}
