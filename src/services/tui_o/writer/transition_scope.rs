//! The scope an operator approves with an Inspect and an Apply must still find: every old hop up to
//! the request's cutoff, whatever its verdict, budget or audit, and one versioned digest of it.

use serde::{Serialize, Serializer};
use sha2::{Digest, Sha256};

use super::{Home, Request, Snapshot, Termination, hop};
use crate::services::tui_o::shadow::{ShadowProvider, SourceId};
use crate::services::tui_o::store::spool::source_key;
use crate::services::tui_o::writer::transition_port::Context;

#[path = "transition_scope/codex.rs"]
pub(crate) mod codex;

const DOMAIN: &[u8] = b"agentdesk/hrot/transition-scope\0";
const VERSION: u8 = 1;
const PREFIX: &str = "hrot-scope-v1:";

/// SHA-256 of one scope's canonical bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ScopeDigest([u8; 32]);

impl ScopeDigest {
    /// A digest as this version shows it; another version or length is refused, never guessed.
    #[cfg(test)]
    pub(crate) fn parse(shown: &str) -> Result<Self, String> {
        let hex = shown.strip_prefix(PREFIX).ok_or("not a v1 scope digest")?;
        let bytes = hex::decode(hex)
            .ok()
            .and_then(|bytes| bytes.try_into().ok());
        Ok(Self(bytes.ok_or("a v1 scope digest is 64 hex digits")?))
    }
}

impl std::fmt::Display for ScopeDigest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{PREFIX}{}", hex::encode(self.0))
    }
}

impl Serialize for ScopeDigest {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// One hop of the scope: its resolved seq and pane, the old source and the one it moved to. The
/// spool key only names the old source for display; the digest takes the source itself.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct ScopeHop {
    pub key: String,
    pub seq: u64,
    pub pane: Option<String>,
    pub old: SourceId,
    pub successor: SourceId,
}

/// The hops a request covers in canonical order, and their digest; the list is only shown.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct ScopeApproval {
    pub digest: ScopeDigest,
    pub hops: Vec<ScopeHop>,
}

/// The scope of `request` in `snapshot`, for the execution and home `context` names once the
/// snapshot verified both; a hop that cannot be placed in or out of it refuses the whole scope.
pub(crate) fn approve(
    snapshot: &Snapshot,
    request: &Request,
    context: &Context,
) -> Result<ScopeApproval, String> {
    if snapshot.provider == ShadowProvider::Codex {
        return Err("Codex apply is deferred; observation identity is not an approval".into());
    }
    if request.channel != snapshot.channel {
        return Err("the request names another channel".into());
    }
    if !matches!(request.termination, Some(Termination::Confirmed(_))) {
        return Err("no checked end: inspect only".into());
    }
    let unverified = |detail: &String| format!("the execution is not verified: {detail}");
    snapshot.pinned.as_ref().map_err(unverified)?;
    snapshot.execution.as_ref().map_err(unverified)?;
    if snapshot.home != Home::Settled {
        return Err("the request's home is not verified".into());
    }
    let mut hops = Vec::new();
    for (key, next) in &snapshot.rotation.successors {
        let (seq, pane) = hop(snapshot, key, next).map_err(|why| format!("hop {key}: {why:?}"))?;
        if seq > request.cutoff {
            continue;
        }
        let old = (snapshot.cursors.iter()).find(|cursor| source_key(&cursor.source) == *key);
        let old = old
            .ok_or(format!("hop {key} has no cursor"))?
            .source
            .clone();
        let successor = next.source.clone();
        hops.push(ScopeHop {
            key: key.clone(),
            seq,
            pane,
            old,
            successor,
        });
    }
    hops.sort_by(|a, b| (a.seq, &a.pane, &a.key).cmp(&(b.seq, &b.pane, &b.key)));
    let bytes = encode(request, snapshot.provider, context, &hops);
    let digest = ScopeDigest(Sha256::digest(&bytes).into());
    Ok(ScopeApproval { digest, hops })
}

/// Domain, version, the request without `at`, the execution, then the hops, each integer
/// big-endian, each string and path its length then its bytes, and each option or variant a tag.
pub(super) fn encode(
    request: &Request,
    provider: ShadowProvider,
    context: &Context,
    hops: &[ScopeHop],
) -> Vec<u8> {
    let mut out = DOMAIN.to_vec();
    out.push(VERSION);
    let int = |out: &mut Vec<u8>, n: u64| out.extend(n.to_be_bytes());
    let bytes = |out: &mut Vec<u8>, b: &[u8]| {
        int(out, b.len() as u64);
        out.extend(b);
    };
    int(&mut out, request.channel);
    int(&mut out, request.cutoff);
    for field in [&request.job_id, &request.operator, &request.reason] {
        bytes(&mut out, field.as_bytes());
    }
    match &request.termination {
        None => out.push(0),
        Some(Termination::Confirmed(end)) => {
            out.push(1);
            bytes(&mut out, end.host.as_bytes());
            bytes(&mut out, end.pane.as_bytes());
            int(&mut out, end.first_seq);
            int(&mut out, end.last_seq);
            bytes(&mut out, end.basis.as_bytes());
        }
        Some(Termination::Implied(basis)) => {
            out.push(2);
            bytes(&mut out, basis.as_bytes());
        }
        Some(Termination::Unread(detail)) => {
            out.push(3);
            bytes(&mut out, detail.as_bytes());
        }
    }
    out.push(match provider {
        ShadowProvider::Claude => 1,
        ShadowProvider::Codex => 2,
    });
    bytes(&mut out, context.tmux.as_bytes());
    bytes(&mut out, context.nonce.as_bytes());
    // Retain the existing v1 NULL home-epoch tag.
    out.push(0);
    int(&mut out, hops.len() as u64);
    let source = |out: &mut Vec<u8>, source: &SourceId| {
        bytes(out, source.session_id.as_bytes());
        bytes(out, source.path.as_os_str().as_encoded_bytes());
        int(out, source.dev);
        int(out, source.ino);
    };
    for hop in hops {
        int(&mut out, hop.seq);
        match &hop.pane {
            None => out.push(0),
            Some(pane) => {
                out.push(1);
                bytes(&mut out, pane.as_bytes());
            }
        }
        source(&mut out, &hop.old);
        source(&mut out, &hop.successor);
    }
    out
}
