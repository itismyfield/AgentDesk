//! Whether anybody currently holds a byte coordinate into the relay jsonl the
//! rotation is about to rewrite from its head (#5452 R2).
//!
//! `truncate_jsonl_head_safe` drops `D` bytes off the front, so every offset into
//! the file shifts by `D` and every offset already past the new EOF stops resolving.
//! That is only safe when no live reader is mid-file, and the reason it needs a gate
//! rather than a lock is that the readers are not all in this process and the writer
//! is in none of them: the wrapper appends from inside tmux, so nothing here can stop
//! it. What can be done is to rotate only at a moment when the coordinate holders are
//! provably at EOF, which is what this module decides.
//!
//! The decision is split in two on purpose. Everything that is a process-local fact
//! is judged here by [`rotation_idle_verdict`], a pure function over evidence
//! [`collect_rotation_idle_evidence`] gathered. Everything that needs the file's
//! length is NOT judged here: those coordinates leave as `eof_witnesses` and are
//! compared inside `truncate_jsonl_head_safe` against the length of the fd it already
//! holds open. A length read here would be a `metadata(path)` before that open, which
//! is the stat/open race #5452 PR-A exists to have closed.
//!
//! What the gate cannot see is a consumer whose operating condition is the absence of
//! an inflight row — `session_relay_sink::run_idle_jsonl_relay_loop` is one, since a
//! row-less channel is what puts it in charge. Term (e) admits exactly that state, so
//! no invariant here is enforced for that loop; the rotation-side mitigation for it is
//! the frontier realignment in the parent module, and it is best-effort.

use crate::services::discord::SharedData;
use crate::services::discord::inflight::{RelayOwnerKind, load_inflight_state};
use crate::services::provider::ProviderKind;
use serenity::model::id::ChannelId;
use std::path::Path;
use std::sync::Arc;

/// The inputs the async half of the rotation hands to the blocking half.
///
/// Everything here is already a local at the call site; the struct exists so the
/// `spawn_blocking` closure moves one value, and so the row and binding reads —
/// disk and a global lock respectively — happen on the blocking side.
pub(super) struct RotationIdleContext {
    pub(super) shared: Arc<SharedData>,
    pub(super) provider: ProviderKind,
    pub(super) channel_id: ChannelId,
    pub(super) current_offset: u64,
    pub(super) all_data_is_empty: bool,
}

/// The inflight row's contribution, folded to values so the verdict stays pure.
pub(super) struct RotationIdleRow {
    /// `effective_relay_owner_kind()`, never the raw field: a legacy row spelling
    /// watcher ownership as `watcher_owns_live_relay` carries `RelayOwnerKind::None`
    /// in the field itself and would pass an owner check that read it directly.
    pub(super) relay_owner_kind: RelayOwnerKind,
    /// `state.last_offset`, and `Some` only when the row's `output_path` is the file
    /// being rotated. A row pointed at some other file holds no coordinate into this
    /// one, so it contributes no witness.
    pub(super) last_offset_into_target: Option<u64>,
}

pub(super) struct RotationIdleEvidence {
    pub(super) current_offset: u64,
    pub(super) all_data_is_empty: bool,
    pub(super) committed_relay_offset: u64,
    pub(super) relay_emission_in_flight: bool,
    pub(super) row: Option<RotationIdleRow>,
    /// `binding.relay_last_offset()` for this tmux session, `Some` only when the
    /// binding's `relay_output_path()` is the file being rotated. The getter and not
    /// the raw `Option` field: it folds to `last_offset` when no relay override is
    /// set, which is the common shape, and reading the raw field would let every
    /// override-less binding through with no witness at all.
    pub(super) binding_relay_offset: Option<u64>,
}

/// Why a rotation was refused, as a value rather than a log string, so the backstop
/// ladder can carry which term is the sticky one instead of just saying "not
/// rotating".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RotationBusyTerm {
    /// Term (b): the watcher is holding read-but-undelivered bytes.
    PendingBuffer,
    /// Term (c): the frontier is already ahead of the reader, so realigning it
    /// comes before shortening the file underneath it.
    FrontierRegressed,
    /// Term (d): a relay POST for this channel is in flight.
    EmissionInFlight,
    /// Term (e): an inflight row names a live relay owner.
    RelayOwner(RelayOwnerKind),
    /// The gate said idle and `truncate_jsonl_head_safe` still declined: a witness
    /// disagreed with the fd's length, the length moved before the rename, or the
    /// entry was swapped. An under-cap file lands here too — the truncate answers the
    /// same "nothing was rewritten" for having no work as for a disagreement — so this
    /// term is counted on ordinary ticks and only the ladder's rungs, which need the
    /// file to be a multiple of the cap, are out of its reach.
    FdRefusal,
    /// Refused before the gate: this jsonl is not AgentDesk's to rewrite (PR-A).
    NotOwned,
}

impl RotationBusyTerm {
    /// Stable label for the ladder's histogram key and log field.
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::PendingBuffer => "pending_buffer",
            Self::FrontierRegressed => "frontier_regressed",
            Self::EmissionInFlight => "emission_in_flight",
            Self::RelayOwner(RelayOwnerKind::None) => "relay_owner:none",
            Self::RelayOwner(RelayOwnerKind::Watcher) => "relay_owner:watcher",
            Self::RelayOwner(RelayOwnerKind::StandbyRelay) => "relay_owner:standby_relay",
            Self::RelayOwner(RelayOwnerKind::SessionBoundRelay) => {
                "relay_owner:session_bound_relay"
            }
            Self::RelayOwner(RelayOwnerKind::Unknown) => "relay_owner:unknown",
            Self::FdRefusal => "fd_refusal",
            Self::NotOwned => "not_owned",
        }
    }
}

pub(super) enum RotationIdleVerdict {
    /// Clear on every process-local term. The coordinates that still need the file's
    /// length travel with the verdict and are settled against the fd.
    Idle {
        eof_witnesses: Vec<u64>,
    },
    Busy(RotationBusyTerm),
}

/// The process-local half of the gate: terms (b) through (e), decided on values.
///
/// Term (e) is an allow-list — a row is admitted only for `RelayOwnerKind::None`,
/// which is the bridge-owned/default shape — and that is the whole point of its
/// spelling. A deny-list of the live variants lets any variant added later pass
/// silently, and it has no arm at all for `None`, which is the `#[default]`.
///
/// A rebind-origin row gets no exemption. The `inflight::model` note that such a row
/// must be treated as absent answers "is there a live foreground turn"; the question
/// here is "does anyone hold a valid byte coordinate into this file", and for a
/// rebind-origin row the answer is yes — `manual_rebind` rebases `last_offset` and
/// plants it, and the recovery and idle-tmux readers then use that value to read this
/// file. Such a row is admitted by (e) and must still satisfy (f) below.
pub(super) fn rotation_idle_verdict(evidence: &RotationIdleEvidence) -> RotationIdleVerdict {
    if !evidence.all_data_is_empty {
        return RotationIdleVerdict::Busy(RotationBusyTerm::PendingBuffer);
    }
    if evidence.committed_relay_offset > evidence.current_offset {
        return RotationIdleVerdict::Busy(RotationBusyTerm::FrontierRegressed);
    }
    if evidence.relay_emission_in_flight {
        return RotationIdleVerdict::Busy(RotationBusyTerm::EmissionInFlight);
    }
    if let Some(row) = &evidence.row
        && row.relay_owner_kind != RelayOwnerKind::None
    {
        return RotationIdleVerdict::Busy(RotationBusyTerm::RelayOwner(row.relay_owner_kind));
    }

    // Term (a) is unconditional and is why the production slice is never empty; (f)
    // and (g) are vacuous when their holder does not exist or is pointed elsewhere.
    let mut eof_witnesses = vec![evidence.current_offset];
    eof_witnesses.extend(
        evidence
            .row
            .as_ref()
            .and_then(|row| row.last_offset_into_target),
    );
    eof_witnesses.extend(evidence.binding_relay_offset);
    RotationIdleVerdict::Idle { eof_witnesses }
}

/// The impure half: read the coordinator, the inflight row and the TUI runtime
/// binding once each, and fold them to values.
///
/// Cost lands only on the ~30s rotation cadence, and the row read is not a new kind
/// of cost — `refresh_watcher_turn_identity` already loads it every tick.
pub(super) fn collect_rotation_idle_evidence(
    context: &RotationIdleContext,
    tmux_session_name: &str,
    rotation_target: &Path,
) -> RotationIdleEvidence {
    let row = load_inflight_state(&context.provider, context.channel_id.get()).map(|state| {
        RotationIdleRow {
            relay_owner_kind: state.effective_relay_owner_kind(),
            last_offset_into_target: state
                .output_path
                .as_deref()
                .filter(|path| names_rotation_target(path, rotation_target))
                .map(|_| state.last_offset),
        }
    });
    let binding =
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(tmux_session_name)
            .filter(|binding| names_rotation_target(binding.relay_output_path(), rotation_target))
            .map(|binding| binding.relay_last_offset());
    RotationIdleEvidence {
        current_offset: context.current_offset,
        all_data_is_empty: context.all_data_is_empty,
        committed_relay_offset: context.shared.committed_relay_offset(context.channel_id),
        relay_emission_in_flight: context.shared.relay_emission_in_flight(context.channel_id),
        row,
        binding_relay_offset: binding,
    }
}

/// Whether `candidate` names the file being rotated. `rotation_target` arrives
/// already resolved — it is the path the ownership verdict was reached on — so the
/// comparison is canonical against canonical.
///
/// A candidate that will not resolve counts as a different file, which admits the
/// rotation rather than blocking it. That direction is sound by elimination, not by
/// optimism: the rotation target at this moment is a path that has resolved and been
/// opened, so a path that does not resolve is not it. What is left is the narrow case
/// of a candidate relinked between resolving here and the fd downstream, which is the
/// same microsecond-scale TOCTOU the append detector's own limit is stated at.
fn names_rotation_target(candidate: &str, rotation_target: &Path) -> bool {
    std::fs::canonicalize(candidate).is_ok_and(|resolved| resolved == rotation_target)
}

#[cfg(test)]
mod rotation_idle_gate_tests {
    use super::*;
    use crate::services::agent_protocol::RuntimeHandoffKind;
    use crate::services::discord::inflight::InflightTurnState;
    use crate::services::tui_prompt_dedupe::TuiRuntimeBinding;

    const READER_AT_EOF: u64 = 4_096;

    /// Everything clear: the shape every other case perturbs by one field.
    fn idle_evidence() -> RotationIdleEvidence {
        RotationIdleEvidence {
            current_offset: READER_AT_EOF,
            all_data_is_empty: true,
            committed_relay_offset: 0,
            relay_emission_in_flight: false,
            row: None,
            binding_relay_offset: None,
        }
    }

    fn busy_term(evidence: &RotationIdleEvidence) -> Option<RotationBusyTerm> {
        match rotation_idle_verdict(evidence) {
            RotationIdleVerdict::Busy(term) => Some(term),
            RotationIdleVerdict::Idle { .. } => None,
        }
    }

    fn witnesses(evidence: &RotationIdleEvidence) -> Vec<u64> {
        match rotation_idle_verdict(evidence) {
            RotationIdleVerdict::Idle { eof_witnesses } => eof_witnesses,
            RotationIdleVerdict::Busy(term) => panic!("expected idle, got {}", term.as_str()),
        }
    }

    /// The truth table for the process-local terms, each perturbation on its own so a
    /// term that stops being consulted fails here rather than silently widening the
    /// window the rotation runs in.
    #[test]
    fn each_process_local_term_refuses_on_its_own() {
        assert_eq!(busy_term(&idle_evidence()), None, "the clear shape is idle");

        let pending = RotationIdleEvidence {
            all_data_is_empty: false,
            ..idle_evidence()
        };
        assert_eq!(
            busy_term(&pending),
            Some(RotationBusyTerm::PendingBuffer),
            "read-but-undelivered bytes would be discarded by the rewrite"
        );

        let regressed = RotationIdleEvidence {
            committed_relay_offset: READER_AT_EOF + 1,
            ..idle_evidence()
        };
        assert_eq!(
            busy_term(&regressed),
            Some(RotationBusyTerm::FrontierRegressed),
            "a frontier already past the reader is realigned before the file shrinks"
        );

        let emitting = RotationIdleEvidence {
            relay_emission_in_flight: true,
            ..idle_evidence()
        };
        assert_eq!(
            busy_term(&emitting),
            Some(RotationBusyTerm::EmissionInFlight),
            "a relay POST is in flight for this channel"
        );

        // A frontier level with the reader is not a regression.
        let level = RotationIdleEvidence {
            committed_relay_offset: READER_AT_EOF,
            ..idle_evidence()
        };
        assert_eq!(busy_term(&level), None);
    }

    /// Term (e) admits only the bridge-owned/default shape, and every live owner is
    /// refused by not being on that list. Spelled as an allow-list precisely so a
    /// `RelayOwnerKind` variant added later is refused by default instead of
    /// slipping through a deny-list that has no arm for it.
    #[test]
    fn only_a_row_with_no_relay_owner_is_admitted() {
        for kind in [
            RelayOwnerKind::Watcher,
            RelayOwnerKind::StandbyRelay,
            RelayOwnerKind::SessionBoundRelay,
            RelayOwnerKind::Unknown,
        ] {
            let owned = RotationIdleEvidence {
                row: Some(RotationIdleRow {
                    relay_owner_kind: kind,
                    last_offset_into_target: None,
                }),
                ..idle_evidence()
            };
            assert_eq!(
                busy_term(&owned),
                Some(RotationBusyTerm::RelayOwner(kind)),
                "{kind:?} owns live delivery"
            );
        }

        let unowned = RotationIdleEvidence {
            row: Some(RotationIdleRow {
                relay_owner_kind: RelayOwnerKind::None,
                last_offset_into_target: None,
            }),
            ..idle_evidence()
        };
        assert_eq!(
            busy_term(&unowned),
            None,
            "a bridge-owned row does not block"
        );
    }

    /// The legacy spelling of watcher ownership: the raw `relay_owner_kind` field is
    /// `None` and `watcher_owns_live_relay` carries the fact instead. The collector
    /// folds it through `effective_relay_owner_kind`, so the verdict sees `Watcher`.
    /// Reading the raw field would admit exactly this row.
    #[test]
    fn a_legacy_watcher_owned_row_is_not_admitted_as_ownerless() {
        let mut state = row_for_test("/tmp/agentdesk-5452-legacy.jsonl", READER_AT_EOF);
        state.watcher_owns_live_relay = true;
        assert_eq!(
            state.effective_relay_owner_kind(),
            RelayOwnerKind::Watcher,
            "the fold is what term (e) depends on"
        );

        let owned = RotationIdleEvidence {
            row: Some(RotationIdleRow {
                relay_owner_kind: state.effective_relay_owner_kind(),
                last_offset_into_target: None,
            }),
            ..idle_evidence()
        };
        assert_eq!(
            busy_term(&owned),
            Some(RotationBusyTerm::RelayOwner(RelayOwnerKind::Watcher))
        );
    }

    /// The production slice is never empty (I-13): term (a) is unconditional, so a
    /// rotation always ships the watcher's own read offset for the fd to check. An
    /// empty slice would rotate with nothing verified at all.
    #[test]
    fn the_reader_offset_is_always_a_witness() {
        assert_eq!(witnesses(&idle_evidence()), vec![READER_AT_EOF]);

        let with_holders = RotationIdleEvidence {
            row: Some(RotationIdleRow {
                relay_owner_kind: RelayOwnerKind::None,
                last_offset_into_target: Some(READER_AT_EOF - 7),
            }),
            binding_relay_offset: Some(READER_AT_EOF - 9),
            ..idle_evidence()
        };
        assert_eq!(
            witnesses(&with_holders),
            vec![READER_AT_EOF, READER_AT_EOF - 7, READER_AT_EOF - 9],
            "each holder of a coordinate into this file contributes one witness"
        );
    }

    /// A rebind-origin row is admitted by term (e) — it names no live relay owner —
    /// and is then held to term (f) like any other. Its `last_offset` is the value
    /// `manual_rebind` planted and that the recovery and idle-tmux readers use to read
    /// this file, so a rotation while it trails the tail must be refused; the refusal
    /// is reached at the fd, on the witness this verdict carries.
    #[test]
    fn a_rebind_origin_row_still_has_to_be_at_eof() {
        let trailing = RotationIdleEvidence {
            row: Some(RotationIdleRow {
                relay_owner_kind: RelayOwnerKind::None,
                last_offset_into_target: Some(READER_AT_EOF - 100),
            }),
            ..idle_evidence()
        };
        assert!(
            witnesses(&trailing).contains(&(READER_AT_EOF - 100)),
            "the row's coordinate must reach the fd check, not be dropped here"
        );
    }

    fn row_for_test(output_path: &str, last_offset: u64) -> InflightTurnState {
        InflightTurnState::new(
            ProviderKind::Claude,
            777,
            None,
            5,
            1001,
            1002,
            "prompt".to_string(),
            None,
            Some("AgentDesk-claude-rot-5452-collect".to_string()),
            Some(output_path.to_string()),
            None,
            last_offset,
        )
    }

    fn binding_for_test(
        output_path: &str,
        relay_output_path: Option<&str>,
        last_offset: u64,
        relay_last_offset: Option<u64>,
    ) -> TuiRuntimeBinding {
        TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            output_path: output_path.to_string(),
            relay_output_path: relay_output_path.map(str::to_string),
            input_fifo_path: None,
            session_id: None,
            last_offset,
            relay_last_offset,
        }
    }

    /// Term (g)'s four shapes, driven through the real collector because what is
    /// under test is which value it reads and whether the path check gates it.
    #[test]
    fn the_tui_binding_contributes_a_witness_only_for_this_file() {
        let _host = crate::config::pin_runtime_host_for_test();

        let session = "AgentDesk-claude-rot-5452-binding";
        let relay = crate::services::tmux_common::session_temp_path(session, "jsonl");
        std::fs::create_dir_all(Path::new(&relay).parent().expect("parent")).expect("create dir");
        std::fs::write(&relay, b"{\"type\":\"assistant\"}\n").expect("write fixture");
        let target = std::fs::canonicalize(&relay).expect("relay resolves");

        let elsewhere = Path::new(&relay).with_extension("other.jsonl");
        std::fs::write(&elsewhere, b"{\"type\":\"assistant\"}\n").expect("write sibling");

        let context = RotationIdleContext {
            shared: crate::services::discord::make_shared_data_for_tests(),
            provider: ProviderKind::Claude,
            channel_id: ChannelId::new(1_479_662_682_909_966_491),
            current_offset: READER_AT_EOF,
            all_data_is_empty: true,
        };
        let collect = || collect_rotation_idle_evidence(&context, session, &target);

        // (iv) No binding at all: vacuously true, so no witness.
        crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(session);
        assert_eq!(
            collect().binding_relay_offset,
            None,
            "no binding, no witness"
        );

        // (i) Bound at this file with a relay override: that override's offset is the
        // witness, and a value short of the tail is what the fd will reject.
        crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
            session,
            binding_for_test(
                &elsewhere.display().to_string(),
                Some(&relay),
                12,
                Some(READER_AT_EOF - 3),
            ),
        );
        assert_eq!(
            collect().binding_relay_offset,
            Some(READER_AT_EOF - 3),
            "the relay coordinate for this file must reach the fd check"
        );

        // (ii) Bound at a different file: no coordinate into this one.
        crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
            session,
            binding_for_test(
                &elsewhere.display().to_string(),
                None,
                READER_AT_EOF - 5,
                None,
            ),
        );
        assert_eq!(
            collect().binding_relay_offset,
            None,
            "a binding pointed at another file holds nothing into this one"
        );

        // (iii) Bound at this file with no override: the getter folds to
        // `last_offset`, so a witness IS carried. Reading the raw `Option` and taking
        // `None` for "no coordinate" would let every override-less binding through.
        crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
            session,
            binding_for_test(&relay, None, READER_AT_EOF - 11, None),
        );
        assert_eq!(
            collect().binding_relay_offset,
            Some(READER_AT_EOF - 11),
            "an unset relay override means the binding's own offset, not no offset"
        );

        crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(session);
    }

    /// Term (f)'s path check, driven through the collector: the row's coordinate
    /// counts only when the row is pointed at the file being rotated.
    #[test]
    fn the_inflight_rows_offset_contributes_a_witness_only_for_this_file() {
        let _host = crate::config::pin_runtime_host_for_test();

        let session = "AgentDesk-claude-rot-5452-row";
        let relay = crate::services::tmux_common::session_temp_path(session, "jsonl");
        std::fs::create_dir_all(Path::new(&relay).parent().expect("parent")).expect("create dir");
        std::fs::write(&relay, b"{\"type\":\"assistant\"}\n").expect("write fixture");
        let target = std::fs::canonicalize(&relay).expect("relay resolves");
        let elsewhere = Path::new(&relay).with_extension("other.jsonl");
        std::fs::write(&elsewhere, b"{\"type\":\"assistant\"}\n").expect("write sibling");

        let channel_id = ChannelId::new(1_479_662_682_909_966_492);
        let context = RotationIdleContext {
            shared: crate::services::discord::make_shared_data_for_tests(),
            provider: ProviderKind::Claude,
            channel_id,
            current_offset: READER_AT_EOF,
            all_data_is_empty: true,
        };
        let collect = || collect_rotation_idle_evidence(&context, session, &target);

        assert!(
            collect().row.is_none(),
            "no row, nothing to hold a coordinate"
        );

        let mut state = row_for_test(&relay, READER_AT_EOF - 4);
        state.channel_id = channel_id.get();
        crate::services::discord::inflight::save_inflight_state(&state).expect("save row");
        let row = collect().row.expect("the row is there");
        assert_eq!(row.relay_owner_kind, RelayOwnerKind::None);
        assert_eq!(
            row.last_offset_into_target,
            Some(READER_AT_EOF - 4),
            "a row pointed at this file carries its coordinate to the fd check"
        );

        let mut state = row_for_test(&elsewhere.display().to_string(), READER_AT_EOF - 4);
        state.channel_id = channel_id.get();
        crate::services::discord::inflight::save_inflight_state(&state).expect("save row");
        let row = collect().row.expect("the row is there");
        assert_eq!(
            row.last_offset_into_target, None,
            "a row pointed at another file holds nothing into this one"
        );
    }
}
