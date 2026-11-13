//! Read-only native membership from the records O already captured; activation supplies the sink.
use super::{Ordered, QueueRecord, SourceBinding, classify_queued_record};
use crate::services::tui_input::human_frame::history::v2::native::{
    Disposition, Ticket, frame_hashes,
};
use serde_json::Value;

#[derive(Clone)]
pub(crate) struct Registration {
    pub(crate) namespace: String,
    pub(crate) ticket: Ticket,
    pub(crate) frame_hash: String,
    pub(crate) receipts: Vec<String>,
}

pub(super) struct Record {
    pub(super) range: (u64, u64),
    pub(super) value: Value,
    pub(super) boundaries: Vec<Ordered>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Run {
    pub(crate) start: u64,
    pub(crate) native_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Evidence {
    pub(crate) namespace: String,
    pub(crate) ticket: Ticket,
    pub(crate) run: Option<Run>,
    pub(crate) range: (u64, u64),
    pub(crate) membership_range: Option<(u64, u64)>,
    pub(crate) membership_row: Option<String>,
    pub(crate) closer_range: Option<(u64, u64)>,
    pub(crate) disposition: Disposition,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Waiting,
    Taken,
    Attached,
    Running,
    Removed,
}
struct Input {
    body: String,
    owner: Option<usize>,
    start: u64,
    stage: Stage,
    fifo: bool,
    command: bool,
    absorbed: bool,
}

#[derive(Clone)]
struct Member {
    run: Run,
    range: (u64, u64),
    row: Option<String>,
}

fn same_body(a: &str, b: &str) -> bool {
    let hashes = frame_hashes(a);
    frame_hashes(b).iter().any(|hash| hashes.contains(hash))
}

pub(super) fn fold(
    binding: &SourceBinding,
    records: &[Record],
    registrations: &[Registration],
) -> Result<Vec<Evidence>, String> {
    if binding.provider != super::ShadowProvider::Claude {
        return Ok(vec![]);
    }
    let matches_source = |r: &&Registration| {
        r.ticket.channel_id == binding.channel_id
            && r.ticket.source.as_ref().is_some_and(|s| {
                s.provider == "claude"
                    && s.session_id == binding.source.session_id
                    && s.transcript == binding.source.path
                    && s.file == [binding.source.dev, binding.source.ino]
                    && s.target == r.ticket.key["attempt"]["target"]
                    && s.target["execution_nonce"]
                        .as_str()
                        .is_some_and(|n| !n.is_empty())
            })
    };
    let registered: Vec<_> = registrations.iter().filter(matches_source).collect();
    let mut inputs: Vec<Input> = vec![];
    let mut run: Option<Run> = None;
    let mut member: Vec<Option<Member>> = vec![None; registered.len()];
    let mut results: Vec<Option<Evidence>> = vec![None; registered.len()];
    let mut ambiguous = vec![false; registered.len()];
    let mut unmatched_dequeue = false;
    for record in records {
        if record.value["sessionId"]
            .as_str()
            .is_some_and(|sid| sid != binding.source.session_id)
        {
            return Err("native record belongs to another source".into());
        }
        for boundary in &record.boundaries {
            match boundary {
                Ordered::Opened {
                    range,
                    native_turn_id,
                } => {
                    run = Some(Run {
                        start: range.0,
                        native_id: native_turn_id.clone(),
                    });
                }
                Ordered::Closed { range, aborted, .. } => {
                    if let Some(closing) = run.take() {
                        for (i, membership) in member.iter().enumerate() {
                            if let Some(owned) = membership
                                && owned.run == closing
                                && !ambiguous[i]
                            {
                                results[i] = Some(Evidence {
                                    namespace: registered[i].namespace.clone(),
                                    ticket: registered[i].ticket.clone(),
                                    run: Some(owned.run.clone()),
                                    range: (owned.range.1, range.1),
                                    membership_range: Some(owned.range),
                                    membership_row: owned.row.clone(),
                                    closer_range: Some(*range),
                                    disposition: if *aborted {
                                        Disposition::Cancelled
                                    } else {
                                        Disposition::Executed
                                    },
                                });
                            }
                        }
                    }
                }
                Ordered::Input { .. } => {}
            }
        }
        let eligible = |i: usize| {
            registered[i]
                .ticket
                .source
                .as_ref()
                .is_some_and(|s| s.offset <= record.range.0)
        };
        let candidate = |body: &str, receipt: bool| {
            let candidates: Vec<_> = registered
                .iter()
                .enumerate()
                .filter(|(i, r)| {
                    eligible(*i)
                        && frame_hashes(body).contains(&r.frame_hash)
                        && (!receipt
                            || record.value["uuid"]
                                .as_str()
                                .is_some_and(|uuid| r.receipts.iter().any(|known| known == uuid)))
                })
                .map(|(i, _)| i)
                .collect();
            if candidates.len() == 1 {
                candidates.first().copied()
            } else {
                None
            }
        };
        let Some(event) = classify_queued_record(&record.value) else {
            continue;
        };
        match event {
            QueueRecord::Enqueue(body) => {
                let owner = candidate(&body, false).filter(|i| {
                    records.iter().filter(|r| r.range.0 >= registered[*i].ticket.source.as_ref().map_or(u64::MAX, |s| s.offset))
                        .filter(|r| matches!(classify_queued_record(&r.value), Some(QueueRecord::Enqueue(ref b)) if same_body(b, &body))).count() == 1
                });
                inputs.push(Input {
                    body,
                    owner,
                    start: record.range.0,
                    stage: Stage::Waiting,
                    fifo: true,
                    command: false,
                    absorbed: false,
                });
            }
            QueueRecord::Dequeue => {
                if let Some(input) = inputs.iter_mut().find(|input| input.fifo) {
                    input.fifo = false;
                    if input.stage == Stage::Waiting {
                        input.stage = Stage::Taken;
                    }
                } else {
                    unmatched_dequeue = true;
                }
            }
            QueueRecord::WithdrawAll => {
                for input in &mut inputs {
                    if matches!(input.stage, Stage::Waiting | Stage::Taken | Stage::Attached) {
                        settle_removed(
                            input,
                            record.range.1,
                            &registered,
                            &mut results,
                            Disposition::WithdrawnNotExecuted,
                        );
                    }
                }
            }
            QueueRecord::Withdraw(body) | QueueRecord::Delivered(body) => {
                let delivered = matches!(
                    classify_queued_record(&record.value),
                    Some(QueueRecord::Delivered(_))
                );
                let matches: Vec<_> = inputs
                    .iter()
                    .enumerate()
                    .filter(|(_, i)| {
                        same_body(&i.body, &body)
                            && matches!(i.stage, Stage::Waiting | Stage::Taken | Stage::Attached)
                    })
                    .map(|(at, _)| at)
                    .collect();
                if let [at] = matches.as_slice() {
                    settle_removed(
                        &mut inputs[*at],
                        record.range.1,
                        &registered,
                        &mut results,
                        if delivered {
                            Disposition::TransferredUnverified
                        } else {
                            Disposition::WithdrawnNotExecuted
                        },
                    );
                    if delivered {
                        inputs[*at].fifo = false;
                    }
                }
            }
            QueueRecord::Absorbed(body)
            | QueueRecord::Human(body)
            | QueueRecord::Command(body)
            | QueueRecord::Prompt(body) => {
                let kind = classify_queued_record(&record.value);
                let absorption = matches!(kind, Some(QueueRecord::Absorbed(_)));
                let command = matches!(kind, Some(QueueRecord::Human(_) | QueueRecord::Command(_)));
                let receipt = candidate(&body, true);
                if receipt.is_none()
                    && (matches!(kind, Some(QueueRecord::Command(_)))
                        || record.value["isMeta"] == true
                        || record.value["isCompactSummary"] == true)
                {
                    continue;
                }
                let matches: Vec<_> = inputs
                    .iter()
                    .enumerate()
                    .filter(|(_, i)| {
                        same_body(&i.body, &body)
                            && i.stage != Stage::Removed
                            && if absorption {
                                !i.absorbed
                            } else if command {
                                !i.command
                            } else {
                                matches!(i.stage, Stage::Taken | Stage::Attached)
                            }
                    })
                    .map(|(at, _)| at)
                    .collect();
                let at = if let [at] = matches.as_slice() {
                    Some(*at)
                } else {
                    None
                };
                let owner = receipt
                    .or_else(|| at.and_then(|at| inputs[at].owner))
                    .or_else(|| {
                        (matches.is_empty()
                            && !absorption
                            && !unmatched_dequeue
                            && !inputs.iter().any(|input| same_body(&input.body, &body)))
                        .then(|| {
                            candidate(&body, false).filter(|i| registered[*i].receipts.is_empty())
                        })
                        .flatten()
                    });
                if let Some(at) = at {
                    let input = &mut inputs[at];
                    input.command |= command;
                    input.absorbed |= absorption;
                    input.fifo = false;
                    input.stage = if run.is_some() {
                        Stage::Running
                    } else {
                        Stage::Attached
                    };
                }
                if let (Some(owner), Some(current)) = (owner, &run) {
                    if let Some(old) = &member[owner] {
                        if old.run != *current || at.is_none() {
                            ambiguous[owner] = true;
                            results[owner] = None;
                        }
                    } else {
                        member[owner] = Some(Member {
                            run: current.clone(),
                            range: record.range,
                            row: record.value["uuid"].as_str().map(str::to_string),
                        });
                    }
                }
            }
        }
    }
    Ok(results
        .into_iter()
        .enumerate()
        .filter_map(|(i, result)| (!ambiguous[i]).then_some(result).flatten())
        .collect())
}

fn settle_removed(
    input: &mut Input,
    end: u64,
    registered: &[&Registration],
    results: &mut [Option<Evidence>],
    disposition: Disposition,
) {
    if let Some(owner) = input.owner {
        results[owner] = Some(Evidence {
            namespace: registered[owner].namespace.clone(),
            ticket: registered[owner].ticket.clone(),
            run: None,
            range: (input.start, end),
            membership_range: None,
            membership_row: None,
            closer_range: None,
            disposition,
        });
    }
    input.stage = Stage::Removed;
}

#[cfg(all(test, unix))]
mod tests;
