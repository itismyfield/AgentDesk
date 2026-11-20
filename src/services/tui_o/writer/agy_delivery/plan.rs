use std::num::NonZeroU64;
use std::ops::Range;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::domain::message_split::split_with_source_ranges;

pub(crate) type Hash = [u8; 32];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TurnKey {
    pub destination: NonZeroU64,
    pub id: NonZeroU64,
    pub nonce: Uuid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TurnLease {
    pub key: TurnKey,
    pub revision: NonZeroU64,
}

/// Terminal output after the spool reader verifies custody and the committed prefix.
pub(crate) struct Seal {
    pub key: TurnKey,
    pub last_seq: u64,
    pub terminal: Terminal,
}

pub(crate) enum Terminal {
    Done {
        conversation: Uuid,
        body: String,
        sha256: Hash,
        split_version: u32,
    },
    Failed,
}

pub(crate) struct Plan {
    pub key: TurnKey,
    pub last_seq: u64,
    pub fingerprint: Hash,
    pub body: Vec<Piece>,
    pub footer: Piece,
}

pub(crate) struct Piece {
    pub source: Range<usize>,
    pub payload: String,
    pub correlation: String,
    pub payload_hash: Hash,
}

impl Plan {
    pub(crate) fn derive(seal: Seal) -> Result<Self, &'static str> {
        if seal.key.nonce.is_nil() || seal.last_seq == 0 {
            return Err("invalid agy seal identity");
        }
        let mut fingerprint = Sha256::new();
        fingerprint.update(seal.last_seq.to_be_bytes());
        let (body, footer) = match seal.terminal {
            Terminal::Done {
                conversation,
                body,
                sha256,
                split_version: 1,
            } if !conversation.is_nil() && !body.trim().is_empty() && hash(&body) == sha256 => {
                fingerprint.update([1]);
                fingerprint.update(conversation.as_bytes());
                fingerprint.update(sha256);
                (split_with_source_ranges(&body), "-# 완료")
            }
            Terminal::Done { .. } => return Err("invalid or unsupported agy Done"),
            Terminal::Failed => (Vec::new(), "-# Antigravity 실행 실패"),
        };
        let mut identity = Sha256::new();
        identity.update(b"agy-delivery-v1");
        identity.update(seal.key.destination.get().to_be_bytes());
        identity.update(seal.key.id.get().to_be_bytes());
        identity.update(seal.key.nonce.as_bytes());

        let terminal: Hash = fingerprint.finalize().into();
        identity.update(terminal);
        let body: Vec<_> = body
            .into_iter()
            .enumerate()
            .map(|(index, (payload, range))| {
                let mut key = identity.clone();
                key.update((index as u64).to_be_bytes());
                Piece::new(key, range, payload)
            })
            .collect();
        identity.update(b"footer");
        let footer = Piece::new(identity, 0..0, footer.into());

        let mut fingerprint = Sha256::new();
        fingerprint.update(terminal);
        for piece in body.iter().chain(std::iter::once(&footer)) {
            fingerprint.update((piece.source.end as u64).to_be_bytes());
            fingerprint.update(piece.payload_hash);
        }
        Ok(Self {
            key: seal.key,
            last_seq: seal.last_seq,
            fingerprint: fingerprint.finalize().into(),
            body,
            footer,
        })
    }
}

impl Piece {
    fn new(key: Sha256, source: Range<usize>, payload: String) -> Self {
        Self {
            source,
            payload_hash: hash(&payload),
            payload,
            correlation: format!("agy:{:x}", key.finalize()),
        }
    }
}

pub(crate) fn hash(text: &str) -> Hash {
    Sha256::digest(text.as_bytes()).into()
}
