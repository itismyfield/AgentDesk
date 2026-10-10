//! Fail-closed decision for one verified provider queue termination.

use std::time::{Duration, Instant};

use crate::services::tui_input::attempt::{Disposition, Effect, QueueEnd, WitnessKind};
use crate::services::tui_input::rows::{Row, RowState};
use crate::services::tui_o::shadow::{ShadowProvider, SourceBinding, SourceId};

pub const SETTLE_WINDOW: Duration = Duration::from_secs(5);
pub const EOF_INTERVAL: Duration = Duration::from_secs(1);

/// Query and decoding errors remain Unknown; only an identity-checked absence is positive.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Absence {
    Absent,
    Present,
    #[default]
    Unknown,
}

#[derive(Clone, Debug)]
pub struct StablePrefix {
    pub source: SourceId,
    pub eof: u64,
    pub digest: String,
    pub complete: bool,
    pub observed_at: Instant,
}

#[derive(Clone, Debug)]
pub struct ActiveEvidence {
    pub binding: SourceBinding,
    pub execution_nonce: String,
    pub observed_at: Instant,
}

/// The caller refreshes lineage, nonce and composer checks while holding the composer lock.
#[derive(Clone, Debug, Default)]
pub struct ResumeEvidence {
    pub old_nonce: String,
    pub old_pane: Absence,
    pub old_pid: Absence,
    pub exited_at: Option<Instant>,
    pub stable_prefixes: Option<[StablePrefix; 2]>,
    pub active: Option<ActiveEvidence>,
    pub lineage_complete: bool,
    pub all_generations_clear: bool,
    pub current_valid: bool,
    pub exact_empty: bool,
    pub control_clear: bool,
    pub settle_profile: String,
}

/// Produces metadata for the next durable Injecting intent; it performs no provider effect.
pub fn decide(
    row: &Row,
    binding: &SourceBinding,
    nonce: &str,
    evidence: &ResumeEvidence,
    now: Instant,
) -> Option<QueueEnd> {
    let prior = row.attempts.last()?;
    let reads = evidence.stable_prefixes.as_ref()?;
    let active = evidence.active.as_ref()?;
    let old_end = reads[1].eof;
    #[cfg(test)]
    if super::super::transition::mutant("resume_settle_only") {
        return now
            .checked_duration_since(active.observed_at)
            .filter(|elapsed| *elapsed >= SETTLE_WINDOW)
            .map(|_| QueueEnd {
                prior_generation: prior.generation,
                old_nonce: prior.execution_nonce.clone(),
                new_nonce: nonce.into(),
                old_source: prior.source.clone(),
                old_end,
                stable_reads: 2,
                settle_profile: evidence.settle_profile.clone(),
            });
    }
    let queued = row.state == RowState::Queued
        && row.received_seq.is_some()
        && prior.effect == Effect::Sent
        && row.witnesses.iter().any(|seen| {
            let witness = &seen.witness;
            witness.generation == prior.generation
                && witness.token == prior.token
                && witness.kind == WitnessKind::Queued
                && witness.range.as_ref().is_some_and(|range| {
                    range.source == prior.source
                        && range.start >= prior.anchor
                        && range.start < range.end
                        && range.end <= old_end
                })
        });
    #[cfg(test)]
    let queued = queued || super::super::transition::mutant("resume_q");
    if !queued {
        return None;
    }
    let exited = !nonce.is_empty()
        && prior.execution_nonce != nonce
        && evidence.old_nonce == prior.execution_nonce
        && evidence.old_pane == Absence::Absent
        && evidence.old_pid == Absence::Absent;
    #[cfg(test)]
    let exited = exited || super::super::transition::mutant("resume_exit");
    if !exited {
        return None;
    }
    let exit = evidence.exited_at?;
    let stable = reads[0].source == prior.source
        && reads[1].source == prior.source
        && reads[0].eof == old_end
        && old_end >= prior.anchor
        && !reads[0].digest.is_empty()
        && reads[0].digest == reads[1].digest
        && reads.iter().all(|read| read.complete)
        && reads[0].observed_at.checked_duration_since(exit).is_some()
        && reads[1]
            .observed_at
            .checked_duration_since(reads[0].observed_at)
            .is_some_and(|elapsed| elapsed >= EOF_INTERVAL)
        && now.checked_duration_since(reads[1].observed_at).is_some()
        && row.witnesses.iter().all(|seen| {
            seen.witness.generation != prior.generation
                || seen
                    .witness
                    .range
                    .as_ref()
                    .is_none_or(|range| range.source == prior.source && range.end <= old_end)
        });
    #[cfg(test)]
    let stable = stable || super::super::transition::mutant("resume_eof");
    if !stable {
        return None;
    }
    let active = binding.provider == ShadowProvider::Claude
        && active.binding == *binding
        && active.execution_nonce == nonce
        && now
            .checked_duration_since(active.observed_at)
            .is_some_and(|elapsed| elapsed >= SETTLE_WINDOW)
        && evidence.lineage_complete
        && evidence.all_generations_clear
        && evidence.current_valid
        && evidence.exact_empty
        && evidence.control_clear
        && !evidence.settle_profile.is_empty()
        && !row.witnesses.iter().any(|seen| {
            (seen.witness.kind.confirms_input() || seen.witness.kind == WitnessKind::Tool) && {
                #[cfg(test)]
                {
                    !super::super::transition::mutant("resume_latest_only")
                        || seen.witness.generation == prior.generation
                }
                #[cfg(not(test))]
                {
                    true
                }
            }
        })
        && !row.dispositions.iter().any(|disposition| {
            matches!(disposition, Disposition::Consumed | Disposition::Cancelled)
        });
    #[cfg(test)]
    let active = active || super::super::transition::mutant("resume_active");
    if !active {
        return None;
    }
    #[cfg(test)]
    if super::super::transition::mutant("resume_always_held") {
        return None;
    }
    Some(QueueEnd {
        prior_generation: prior.generation,
        old_nonce: prior.execution_nonce.clone(),
        new_nonce: nonce.into(),
        old_source: prior.source.clone(),
        old_end,
        stable_reads: 2,
        settle_profile: evidence.settle_profile.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::tui_input::attempt::{AttemptMeta, Seen, Witness};
    use crate::services::tui_o::shadow::SourceRange;
    use serde_json::json;

    struct Fixture {
        row: Row,
        binding: SourceBinding,
        evidence: ResumeEvidence,
        now: Instant,
    }

    fn fixture() -> Fixture {
        let now = Instant::now();
        let source = SourceId {
            session_id: "old".into(),
            path: "old.jsonl".into(),
            dev: 1,
            ino: 2,
        };
        let binding = SourceBinding {
            channel_id: 77,
            provider: ShadowProvider::Claude,
            source: SourceId {
                session_id: "new".into(),
                path: "new.jsonl".into(),
                dev: 1,
                ino: 3,
            },
        };
        let prior = AttemptMeta {
            generation: 1,
            token: "1".repeat(32),
            frame_digest: "a".repeat(64),
            frame_profile: None,
            execution_nonce: "old-nonce".into(),
            source: source.clone(),
            anchor: 20,
            effect: Effect::Sent,
            incarnation: None,
            queue_end: None,
        };
        let row = Row {
            since_seq: 1,
            received_seq: Some(1),
            state: RowState::Queued,
            attempt: None,
            input: json!({"text":"hello"}),
            attempts: vec![prior],
            witnesses: vec![Seen {
                witness: Witness {
                    generation: 1,
                    token: "1".repeat(32),
                    kind: WitnessKind::Queued,
                    range: Some(SourceRange {
                        source: source.clone(),
                        start: 20,
                        end: 40,
                    }),
                    record_key: None,
                    turn_ref: None,
                },
                late: false,
            }],
            dispositions: vec![],
            retention: None,
        };
        let first = StablePrefix {
            source,
            eof: 40,
            digest: "b".repeat(64),
            complete: true,
            observed_at: now - EOF_INTERVAL,
        };
        let mut second = first.clone();
        second.observed_at = now;
        let evidence = ResumeEvidence {
            old_nonce: "old-nonce".into(),
            old_pane: Absence::Absent,
            old_pid: Absence::Absent,
            exited_at: Some(first.observed_at),
            stable_prefixes: Some([first, second]),
            active: Some(ActiveEvidence {
                binding: binding.clone(),
                execution_nonce: "new-nonce".into(),
                observed_at: now - SETTLE_WINDOW,
            }),
            lineage_complete: true,
            all_generations_clear: true,
            current_valid: true,
            exact_empty: true,
            control_clear: true,
            settle_profile: "claude-2.1.295-eof1-settle5".into(),
        };
        Fixture {
            row,
            binding,
            evidence,
            now,
        }
    }

    fn decide_fixture(fixture: &Fixture) -> Option<QueueEnd> {
        decide(
            &fixture.row,
            &fixture.binding,
            "new-nonce",
            &fixture.evidence,
            fixture.now,
        )
    }

    #[test]
    fn verified_queue_end_at_exact_time_boundaries() {
        let fixture = fixture();
        let end = decide_fixture(&fixture).unwrap();
        assert_eq!(end.prior_generation, 1);
        assert_eq!(end.old_end, 40);
        assert_eq!(end.stable_reads, 2);
        assert_eq!(end.old_nonce, "old-nonce");
        assert_eq!(end.new_nonce, "new-nonce");
    }

    #[test]
    fn durable_q_guard_rejects_hook_only_and_unregistered_q() {
        let mut fixture = fixture();
        fixture.row.witnesses[0].witness.kind = WitnessKind::Hook;
        assert!(decide_fixture(&fixture).is_none());
        fixture.row.witnesses[0].witness.kind = WitnessKind::Queued;
        fixture.row.witnesses[0].witness.token = "2".repeat(32);
        assert!(decide_fixture(&fixture).is_none());
        fixture.row.witnesses[0].witness.token = "1".repeat(32);
        fixture.row.witnesses[0].witness.range = None;
        assert!(decide_fixture(&fixture).is_none());
    }

    #[test]
    fn old_exit_guard_rejects_present_unknown_and_decode_failure() {
        for absence in [Absence::Present, Absence::Unknown] {
            let mut fixture = fixture();
            fixture.evidence.old_pane = absence;
            assert!(decide_fixture(&fixture).is_none());
            fixture.evidence.old_pane = Absence::Absent;
            fixture.evidence.old_pid = absence;
            assert!(decide_fixture(&fixture).is_none());
        }
        // A failed ps decoder never yields a positive process absence.
        assert_eq!(Absence::default(), Absence::Unknown);
        let mut fixture = fixture();
        fixture.evidence.old_nonce = "different-old-episode".into();
        assert!(decide_fixture(&fixture).is_none());
        fixture.evidence.old_nonce = "old-nonce".into();
        fixture.evidence.exited_at = None;
        assert!(decide_fixture(&fixture).is_none());
    }

    #[test]
    fn stable_eof_guard_rejects_changed_identity_bytes_and_partial_tail() {
        let changes: [fn(&mut StablePrefix); 4] = [
            |read| read.source.ino += 1,
            |read| read.eof += 1,
            |read| read.digest.push('c'),
            |read| read.complete = false,
        ];
        for change in changes {
            let mut fixture = fixture();
            change(&mut fixture.evidence.stable_prefixes.as_mut().unwrap()[1]);
            assert!(decide_fixture(&fixture).is_none());
        }
        let mut fixture = fixture();
        fixture.evidence.stable_prefixes = None;
        assert!(decide_fixture(&fixture).is_none());
    }

    #[test]
    fn active_guard_rejects_missing_active_and_invalid_current_checks() {
        let changes: [fn(&mut ResumeEvidence); 7] = [
            |e| e.active = None,
            |e| e.lineage_complete = false,
            |e| e.all_generations_clear = false,
            |e| e.current_valid = false,
            |e| e.exact_empty = false,
            |e| e.control_clear = false,
            |e| e.active.as_mut().unwrap().execution_nonce = "other".into(),
        ];
        for change in changes {
            let mut fixture = fixture();
            change(&mut fixture.evidence);
            assert!(decide_fixture(&fixture).is_none());
        }
        let mut fixture = fixture();
        fixture.evidence.active.as_mut().unwrap().binding.source.ino += 1;
        assert!(decide_fixture(&fixture).is_none());
    }

    #[test]
    fn time_just_below_boundaries_and_regressions_are_closed() {
        let epsilon = Duration::from_nanos(1);
        let mut fixture = fixture();
        fixture.evidence.active.as_mut().unwrap().observed_at += epsilon;
        assert!(decide_fixture(&fixture).is_none());
        fixture.evidence.active.as_mut().unwrap().observed_at = fixture.now + epsilon;
        assert!(decide_fixture(&fixture).is_none());
        let mut fixture = super::tests::fixture();
        fixture.evidence.stable_prefixes.as_mut().unwrap()[0].observed_at += epsilon;
        assert!(decide_fixture(&fixture).is_none());
        let reads = fixture.evidence.stable_prefixes.as_mut().unwrap();
        reads[0].observed_at = fixture.now;
        reads[1].observed_at = fixture.now - epsilon;
        assert!(decide_fixture(&fixture).is_none());
        let mut fixture = super::tests::fixture();
        fixture.evidence.exited_at = Some(fixture.now);
        assert!(decide_fixture(&fixture).is_none());
        fixture.evidence.exited_at = Some(fixture.now - EOF_INTERVAL);
        fixture.evidence.stable_prefixes.as_mut().unwrap()[1].observed_at = fixture.now + epsilon;
        assert!(decide_fixture(&fixture).is_none());
    }

    #[test]
    fn any_generation_delivery_or_tool_consumption_prevents_reoffer() {
        for kind in [
            WitnessKind::User,
            WitnessKind::Attachment,
            WitnessKind::Tool,
        ] {
            let mut fixture = fixture();
            let mut previous = fixture.row.attempts[0].clone();
            previous.token = "0".repeat(32);
            fixture.row.attempts.insert(0, previous);
            fixture.row.attempts.last_mut().unwrap().generation = 2;
            fixture.row.witnesses[0].witness.generation = 2;
            let mut older = fixture.row.witnesses[0].clone();
            older.witness.generation = 1;
            older.witness.token = "0".repeat(32);
            older.witness.kind = kind;
            fixture.row.witnesses.push(older);
            assert!(decide_fixture(&fixture).is_none());
        }
        let mut fixture = fixture();
        fixture.row.dispositions.push(Disposition::Consumed);
        assert!(decide_fixture(&fixture).is_none());
    }

    #[test]
    fn durable_next_generation_prevents_same_episode_replay() {
        let mut fixture = fixture();
        let end = decide_fixture(&fixture).unwrap();
        let mut next = fixture.row.attempts[0].clone();
        next.generation = 2;
        next.execution_nonce = end.new_nonce.clone();
        next.token = "2".repeat(32);
        next.effect = Effect::Intent;
        next.queue_end = Some(end);
        fixture.row.attempts.push(next);
        fixture.row.state = RowState::Injecting;
        assert!(decide_fixture(&fixture).is_none());
        fixture.row.state = RowState::Queued;
        fixture.row.attempts.last_mut().unwrap().effect = Effect::Sent;
        let mut q = fixture.row.witnesses[0].clone();
        q.witness.generation = 2;
        q.witness.token = "2".repeat(32);
        fixture.row.witnesses.push(q);
        assert!(decide_fixture(&fixture).is_none());
    }
}
