//! Adoption of a Codex channel that already holds output, dormant: one evaluator judges whether O
//! may start at Legacy's settled boundaries. Nothing here reads runtime state or writes a store.

pub mod coord;
pub mod fold;
pub mod judge;
pub mod plan;
pub mod probe;
pub mod scan;

use std::collections::BTreeMap;

use serde::Deserialize;

/// One channel's adoption evidence, as a probe snapshot states it or a verifier assembles it. A
/// field the producer did not fill reads as Unknown, never as passing.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Evidence {
    pub channel: u64,
    pub provider: Provider,
    pub runtime_kind: RuntimeKind,
    pub role: Role,
    pub store: StoreState,
    pub candidate: CandidateState,
    pub sources: Vec<SourceEvidence>,
    pub discovery: Step,
    pub recovery: Step,
    pub obligations: BTreeMap<Obligation, Load>,
    pub emission_epoch: Option<u64>,
    pub anchor: Anchor,
}

/// One source the channel's binding log names, read on its own coordinates.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct SourceEvidence {
    pub role: SourceRole,
    /// The file's end `B` as read; `None` when it could not be read.
    pub bytes: Option<u64>,
    pub proof: Proof,
    /// Legacy's proof-linked native checkpoint `C`: where it stopped consuming, not delivering.
    pub native_cursor: Option<u64>,
    pub prefix: Parse,
    /// The records in `[C, B)`.
    pub suffix: Suffix,
    pub closed: Closed,
    pub retirement: Retirement,
    pub wrapper: Option<WrapperEvidence>,
    /// Whether Legacy's receipts cover every unit to `B`; only the strict rule asks.
    pub receipts: Coverage,
}

/// A normalized wrapper spool, judged in its own coordinates and never by the native cursor.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct WrapperEvidence {
    pub proof: Proof,
    pub cursor: Option<u64>,
    pub eof: Option<u64>,
    /// The confirmed delivery floor in the wrapper's coordinates.
    pub floor: Option<u64>,
    /// The records in `[floor, eof)`.
    pub suffix: Suffix,
    pub backlog: Load,
}

macro_rules! evidence_enum {
    ($(#[$meta:meta])* $name:ident { $($variant:ident),* $(,)? }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name {
            $($variant,)*
            /// Not stated, or stated in a form this evaluator does not know.
            #[default]
            #[serde(other)]
            Unknown,
        }

        impl $name {
            /// Every stated variant.
            pub const ALL: &[$name] = &[$($name::$variant),*];
        }
    };
}

evidence_enum!(Provider { Codex, Claude });
evidence_enum!(RuntimeKind {
    CodexTui,
    ClaudeTui
});
evidence_enum!(
    /// The boot role whose recovery the witness speaks for; only Gateway and RestWorker restore.
    Role { Gateway, RestWorker, Standby, Utility }
);
evidence_enum!(
    /// `Fresh`: neither era nor init names the channel.
    StoreState { Fresh, Store, Held }
);
evidence_enum!(CandidateState {
    Pending,
    Committed,
    Held,
    Released,
    Deferred
});
evidence_enum!(
    /// A boot step: `Spawned` and `Skipped` say a task started or was left out, not that it ended.
    Step { Complete, Spawned, Skipped, Timeout }
);
evidence_enum!(Load { Clear, Busy });
evidence_enum!(
    /// What Legacy may still owe a channel; every kind must read Clear before adoption.
    Obligation {
        Mailbox,
        Custody,
        NativeTail,
        Receiver,
        NormalizedBacklog,
        Bridge,
        Finalizer,
        QueuedTerminal,
        DeliveryLease,
        Emission,
        DeadLetter,
    }
);
evidence_enum!(SourceRole {
    Current,
    Retired,
    Named
});
evidence_enum!(
    /// `Unlinked`: no proof-linked checkpoint exists for the source.
    Proof { Linked, Unlinked, Mismatch }
);
evidence_enum!(Parse { Strict, Malformed });
evidence_enum!(
    /// `Unrecognized`: a record outside the allowed non-output list; `Partial`: a cut line or a
    /// cursor off a record end.
    Suffix { Quiet, Prompt, Output, Start, Partial, Unrecognized }
);
evidence_enum!(
    /// `Own`: the last turn ended by its own completion with nothing open after it.
    Closed { Own, Open }
);
evidence_enum!(
    /// `Replaced`: a later bind on the same pane and nonce named it old; `Exited`: its nonce ended.
    /// `Live`: a live binding still reads it.
    Retirement { Replaced, Exited, Live, Missing }
);
evidence_enum!(Coverage { Complete, Gap });

/// The destination channel's newest message id, read after Legacy settled.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Anchor {
    Latest {
        id: u64,
    },
    /// The read proved the channel holds no message.
    Empty,
    /// A probe that does not read Discord.
    Skipped,
    #[default]
    #[serde(other)]
    Unknown,
}

#[cfg(test)]
#[path = "codex_adoption/judge_tests.rs"]
mod judge_tests;
#[cfg(test)]
#[path = "codex_adoption/plan_tests.rs"]
mod plan_tests;
#[cfg(test)]
#[path = "codex_adoption/probe_tests.rs"]
mod probe_tests;
