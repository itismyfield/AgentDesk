//! The requester capability: 256 random bits the asking CLI keeps on its own host. The server
//! stores only its hash, with the request's agent and channel as its scope.

use sha2::{Digest, Sha256};

/// The request header that carries the capability.
pub(crate) const HEADER: &str = "x-agentdesk-decision-owner";

/// Whether `text` has the shape of a capability: 64 lowercase hex digits.
pub(crate) fn well_formed(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The hash stored for a presented capability; `None` when it is not a capability at all.
pub(crate) fn hash(presented: &str) -> Option<String> {
    well_formed(presented).then(|| hex::encode(Sha256::digest(presented.as_bytes())))
}
