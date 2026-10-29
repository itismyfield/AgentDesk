//! Reconcile only the frozen legacy cohort; receipt imports never prepare a POST.

use super::{ChannelWriter, Step};
use crate::services::tui_o::shadow::{SourceRange, UnitKey};
use crate::services::tui_o::store::ledger::input_notices::{
    MigrationHistory, MigrationSnapshot, NoticeEntry, NoticeEvidence, NoticeState,
};
use crate::services::tui_o::store::ledger::{LedgerEntry, PieceOutcome};
use crate::services::tui_o::writer::input_notices::evidence::before_migration_floor;
use crate::services::tui_o::writer::{
    AlarmSink, DeliveryLease, DiscordPort, SeenMessage, WriterAlarm, confirm,
};
use std::collections::{BTreeSet, HashMap};

#[derive(Default)]
struct Plan {
    entries: Vec<NoticeEntry>,
    ready: Vec<UnitKey>,
}

fn member(snapshot: &MigrationSnapshot, record: &SourceRange) -> bool {
    snapshot.sources.iter().any(|source| {
        source.source == record.source
            && source.lower <= record.start
            && record.start < source.upper
    })
}

fn complete(input: &NoticeEvidence) -> bool {
    input.unit_key.is_some() && input.payload.is_some() && input.origin.is_some()
}

#[derive(Default)]
struct History {
    messages: Vec<SeenMessage>,
    error: Option<String>,
}

async fn history<P: DiscordPort>(
    port: &P,
    channel: u64,
    range: &MigrationHistory,
    proof_ids: &[u64],
) -> History {
    let mut messages = Vec::new();
    let result = async {
        let (mut after, upper) = match range {
            MigrationHistory::Fixed { lower, upper } => (*lower, *upper),
            MigrationHistory::Unavailable { reason } => return Err(reason.clone()),
        };
        if after == upper {
            return Ok(());
        }
        let mut listed = false;
        for _ in 0..confirm::MAX_PAGES {
            let mut page = confirm::bounded(port.history_after(channel, after)).await?;
            page.sort_by_key(|message| message.id);
            if page.len() > confirm::HISTORY_PAGE
                || page.iter().any(|message| message.id <= after)
                || page.windows(2).any(|pair| pair[0].id == pair[1].id)
            {
                return Err("invalid legacy history page".into());
            }
            listed |= !page.is_empty();
            messages.extend(
                page.iter()
                    .filter(|message| message.id <= upper && message.author_id == port.bot_id())
                    .cloned(),
            );
            if page.last().is_none_or(|last| last.id >= upper) || page.len() < confirm::HISTORY_PAGE
            {
                if !listed
                    && confirm::first_proof(
                        proof_ids,
                        |id| port.message(channel, id),
                        |id, read| (read.id == id && read.author_id == port.bot_id()).then_some(()),
                    )
                    .await?
                    .is_none()
                {
                    return Err("empty legacy history without permission proof".into());
                }
                return Ok(());
            }
            if let Some(last) = page.last() {
                after = last.id;
            }
        }
        Err("legacy history exceeded its page budget".into())
    }
    .await;
    History {
        messages,
        error: result.err(),
    }
}

async fn plan<P: DiscordPort>(
    port: &P,
    channel: u64,
    state: &NoticeState,
    cohort_complete: bool,
    (pending, eligible, deferred): (&[UnitKey], &[UnitKey], &[UnitKey]),
    proof_ids: &[u64],
) -> Plan {
    let Some(snapshot) = state.migration.as_ref() else {
        return Plan::default();
    };
    let eligible_input = |input: &NoticeEvidence| {
        complete(input)
            && input
                .unit_key
                .as_ref()
                .is_some_and(|key| eligible.contains(key))
    };
    let cohort: Vec<_> = state
        .evidence
        .values()
        .filter(|input| member(snapshot, &input.record))
        .collect();
    let mut groups: HashMap<&str, BTreeSet<&UnitKey>> = HashMap::new();
    for input in &cohort {
        if let (Some(payload), Some(key)) = (input.payload.as_deref(), input.unit_key.as_ref()) {
            groups.entry(payload).or_default().insert(key);
        }
    }
    let blocked: BTreeSet<_> = state
        .unresolved
        .keys()
        .filter_map(|id| state.evidence.get(id)?.unit_key.as_ref())
        .collect();
    let work: Vec<_> = state
        .evidence
        .iter()
        .filter(|(id, input)| {
            !before_migration_floor(state, &input.record)
                && !state.unresolved.contains_key(*id)
                && input
                    .unit_key
                    .as_ref()
                    .is_none_or(|key| !state.imported.contains_key(key) && !blocked.contains(key))
        })
        .map(|(_, input)| input)
        .collect();
    let history = if work.iter().any(|input| {
        member(snapshot, &input.record)
            && eligible_input(input)
            && input
                .unit_key
                .as_ref()
                .is_none_or(|key| !pending.contains(key))
    }) {
        history(port, channel, &snapshot.history, proof_ids).await
    } else {
        History::default()
    };
    let all_complete = cohort_complete && cohort.iter().all(|input| complete(input));
    let mut entries = Vec::new();
    let mut ready = BTreeSet::new();
    let mut imported = BTreeSet::new();
    for input in work {
        let queued = input
            .unit_key
            .as_ref()
            .is_some_and(|key| pending.contains(key));
        // Undecided injection ownership is judged once a receipt or withdrawal settles it.
        let undecided = (input.unit_key.as_ref()).is_some_and(|key| deferred.contains(key));
        if member(snapshot, &input.record) && undecided && !queued {
            continue;
        }
        if !member(snapshot, &input.record) || queued {
            if eligible_input(input)
                && let Some(key) = input.unit_key.as_ref()
            {
                ready.insert(key.clone());
            }
            continue;
        }
        let candidates: Vec<_> = history
            .messages
            .iter()
            .filter(|message| input.payload.as_ref() == Some(&message.content))
            .map(|message| message.id)
            .collect();
        let reason = if !eligible_input(input) {
            Some("legacy source policy is not eligible".to_owned())
        } else if !all_complete {
            Some("legacy cohort is incomplete".into())
        } else {
            history.error.clone()
        };
        if reason.is_none()
            && let Some(key) = input.unit_key.as_ref()
        {
            if candidates.is_empty() {
                ready.insert(key.clone());
                continue;
            }
            if let [id] = candidates.as_slice()
                && input
                    .payload
                    .as_deref()
                    .and_then(|payload| groups.get(payload))
                    .is_some_and(|keys| keys.len() == 1)
                && !state.reserved_ids.contains(id)
                && !state.imported.values().any(|reserved| reserved == id)
            {
                if imported.insert(key.clone()) {
                    entries.push(NoticeEntry::LegacyImported {
                        input_key: key.clone(),
                        msg_id: *id,
                    });
                }
                continue;
            }
        }
        entries.push(NoticeEntry::MigrationUnresolved {
            record: input.record.clone(),
            reason: reason.unwrap_or_else(|| "legacy receipt is not uniquely attributable".into()),
            candidates,
        });
    }
    Plan {
        entries,
        ready: ready.into_iter().collect(),
    }
}

impl<P: DiscordPort, L: DeliveryLease, A: AlarmSink> ChannelWriter<P, L, A> {
    pub(crate) async fn reconcile_notice_migration(
        &mut self,
        complete: bool,
        pending: &[UnitKey],
        eligible: &[UnitKey],
        deferred: &[UnitKey],
    ) -> Result<Vec<UnitKey>, Step> {
        if self.is_stopped() {
            return Err(Step::Stopped);
        }
        if let Some(detail) = self.store.ledger().violation().map(str::to_owned) {
            return Err(self.stop(WriterAlarm::LedgerViolation { detail }));
        }
        let ledger = self.store.ledger();
        let terminal = |key: &UnitKey| {
            ledger.excluded(key).is_some()
                || ledger.latest_piece(key, 0).is_some_and(|(_, piece)| {
                    matches!(piece.outcome, Some(PieceOutcome::Posted(_)))
                })
        };
        let pending: Vec<_> = pending
            .iter()
            .chain(
                ledger
                    .input_notices()
                    .evidence
                    .values()
                    .filter_map(|input| input.unit_key.as_ref())
                    .filter(|key| terminal(key)),
            )
            .cloned()
            .collect();
        let eligible: Vec<_> = eligible
            .iter()
            .filter(|key| !terminal(key))
            .cloned()
            .collect();
        let proof_ids: Vec<_> = (0..ledger.next_serial())
            .rev()
            .filter_map(|serial| match ledger.piece(serial)?.outcome {
                Some(PieceOutcome::Posted(id)) => Some(id),
                _ => None,
            })
            .take(confirm::PROOF_IDS)
            .collect();
        let planned = plan(
            &*self.port,
            self.channel,
            ledger.input_notices(),
            complete,
            (&pending, &eligible, deferred),
            &proof_ids,
        )
        .await;
        for entry in planned.entries {
            self.record(LedgerEntry::InputNotice {
                entry: Box::new(entry),
            })?;
        }
        Ok(planned.ready)
    }
}

#[cfg(test)]
#[path = "migration_tests.rs"]
mod tests;
