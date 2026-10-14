//! The judgment's inputs as the actor holds them: its cursors and readers, what it owes, the
//! stored rotation, the pane's current execution, and the probes a job took so far.

use std::collections::{BTreeMap, BTreeSet};

use super::*;
use crate::services::claude_tui::source_verify::SourceHistory;
use crate::services::cluster::channel_home::{self, HomeIntake, HomeOwnership};
use crate::services::tui_o::ownership::{GatewayOwnership, OwnershipGate};
use crate::services::tui_o::shadow::SourceId;
use crate::services::tui_o::shadow::capture::Fingerprint;
use crate::services::tui_o::store::rotation::{Rotation, Successor};
use crate::services::tui_o::store::spool::Cursor;
use crate::services::tui_o::writer::transition_cleanup::observe::Observation;
use crate::services::tui_o::writer::transition_cleanup::{
    Home, Obligations, ReaderView, Snapshot, SourceFacts,
};
use crate::services::tui_o::writer::transition_port::{Context, Diagnostics, HistoryView, Overlap};
use crate::services::tui_prompt_dedupe::binding_context::{
    SpawnNonceMarker, observe_spawn_nonce_marker,
};
use crate::services::tui_prompt_dedupe::binding_events;

/// The pane's pinned source and its history in the request's execution.
pub(super) type Execution = Result<(Option<SourceId>, SourceHistory), String>;

/// What the probes of every source assumed; a change discards all of them.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Common {
    rotation: Rotation,
    checkpoint: Option<u64>,
    execution: Execution,
}

/// What the probes of one source assumed: its whole cursor, its reader and FD, and its successor.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Token {
    pub cursor: Cursor,
    reader: Option<(ReaderView, Option<Result<Fingerprint, String>>)>,
    successor: Option<Successor>,
}

/// The two probes of one source, both taken for `token`.
pub(super) struct Probe {
    pub token: Token,
    pub earlier: Option<Observation>,
    pub later: Option<Observation>,
}

impl Common {
    /// The execution `snapshot` was judged against, read with `nonce`, never read again.
    pub(super) fn diagnostics(&self, snapshot: &Snapshot, nonce: &str) -> Diagnostics {
        let history = (self.execution.as_ref()).map(|(pinned, history)| HistoryView {
            complete: history.complete,
            pinned: pinned.clone(),
            sessions: sessions(history),
        });
        let overlaps =
            (history.as_ref()).map_or_else(|_| Vec::new(), |view| overlaps(snapshot, view));
        Diagnostics {
            nonce: nonce.to_owned(),
            history: history.map_err(Clone::clone),
            overlaps,
        }
    }
}

fn sessions(history: &SourceHistory) -> Vec<(&'static str, String, Option<SourceId>)> {
    let visits = [
        ("current", &history.current),
        ("awaiting", &history.awaiting),
    ];
    let visits = visits.into_iter().filter_map(|(role, visit)| {
        (visit.as_ref()).map(|visit| (role, visit.session.clone(), visit.pin.clone()))
    });
    let left =
        (history.left.iter()).map(|(session, left)| ("left", session.clone(), left.pin.clone()));
    visits.chain(left).collect()
}

/// Every old source a pin may be, compared as the judgment's current-source check compares, and
/// every one an unpinned session may be, as `execution_sources` refuses it.
fn overlaps(snapshot: &Snapshot, view: &HistoryView) -> Vec<Overlap> {
    let pinned = (view.sessions.iter()).filter_map(|(_, _, pin)| pin.as_ref());
    let pins: Vec<&SourceId> = view.pinned.iter().chain(pinned).collect();
    let old =
        |cursor: &&Cursor| (snapshot.rotation.successors).contains_key(&source_key(&cursor.source));
    let overlap = |cursor: &Cursor| {
        let old = &cursor.source;
        let same = |pin: &SourceId| {
            pin.path == old.path || pin.session_id == old.session_id || pin.ino == old.ino
        };
        let mut held_by: Vec<SourceId> = Vec::new();
        for pin in pins.iter().copied().filter(|pin| same(pin)) {
            if !held_by.contains(pin) {
                held_by.push(pin.clone());
            }
        }
        let named = |session: &String| old.session_id.is_empty() || *session == old.session_id;
        let unpinned = (view.sessions.iter())
            .filter(|(_, session, pin)| pin.is_none() && named(session))
            .map(|(_, session, _)| session.clone());
        let unpinned: Vec<String> = unpinned.collect();
        let key = source_key(old);
        (!held_by.is_empty() || !unpinned.is_empty()).then_some(Overlap {
            key,
            held_by,
            unpinned,
        })
    };
    snapshot
        .cursors
        .iter()
        .filter(old)
        .filter_map(overlap)
        .collect()
}

impl<P: DiscordPort, L: DeliveryLease, A: AlarmSink, B: BindingEvents> Actor<P, L, A, B> {
    /// A store read that fails, or disagrees with what the readers act on, refuses the snapshot.
    pub(super) fn build_snapshot(
        &mut self,
        context: &Context,
        gate: &OwnershipGate,
        probes: &BTreeMap<String, Probe>,
    ) -> Result<(Snapshot, Common), String> {
        #[cfg(test)]
        super::transition::test_hooks::snapshot_built();
        let channel = self.writer.channel();
        let (rotation, checkpoint) = self.sources.transition_rotation();
        let (rotation, checkpoint) = (rotation.clone(), checkpoint);
        let cursors: Vec<Cursor> = self.writer.store().cursors().cloned().collect();
        let old = |cursor: &&Cursor| {
            rotation
                .successors
                .contains_key(&source_key(&cursor.source))
        };
        let store = self.writer.store();
        let facts = store.read_transition_facts(cursors.iter().filter(old).map(|c| &c.source));
        if facts.rotation? != rotation {
            return Err("the stored rotation is not the one the readers act on".into());
        }
        if facts.checkpoint? != checkpoint {
            return Err("the stored binding checkpoint is not the readers'".into());
        }
        let execution = execution_read(channel, context);
        let sessions = (cursors.iter().filter(old)).map(|c| c.source.session_id.as_str());
        let sessions: BTreeSet<&str> = sessions.collect();
        let held =
            (execution.clone()).and_then(|(_, history)| execution_sources(&history, &sessions));
        let mut sources = BTreeMap::new();
        for cursor in &cursors {
            let key = source_key(&cursor.source);
            let probe = probes.get(&key);
            let witness = facts.witnesses.get(&key).cloned().transpose()?.flatten();
            let gc = self.writer.store().ledger().gc_segments(&cursor.source);
            let facts = SourceFacts {
                reader: self
                    .sources
                    .reader_view(&cursor.source)
                    .map(|(view, _)| view),
                gc_logged: !gc.is_empty(),
                witness,
                earlier: probe.and_then(|probe| probe.earlier.clone()),
                later: probe.and_then(|probe| probe.later.clone()),
            };
            sources.insert(key, facts);
        }
        let legacy_hops = self.sources.legacy_candidates(&mut self.writer);
        let snapshot = Snapshot {
            channel,
            provider: self.sources.transition_provider(),
            home: home_now(channel, gate, context.home_epoch),
            rotation: rotation.clone(),
            cursors,
            pinned: execution.clone().map(|(pin, _)| pin),
            execution: held,
            sources,
            legacy_hops,
            obligations: self.obligations(),
        };
        let common = Common {
            rotation,
            checkpoint,
            execution,
        };
        Ok((snapshot, common))
    }

    pub(super) fn token(&self, cursor: &Cursor) -> Token {
        let key = source_key(&cursor.source);
        Token {
            cursor: cursor.clone(),
            reader: self.sources.reader_view(&cursor.source),
            successor: self
                .sources
                .transition_rotation()
                .0
                .successors
                .get(&key)
                .cloned(),
        }
    }

    /// What the actor and the channel's POSTs still owe; an unread POST count is an error.
    pub(super) fn obligations(&mut self) -> Result<Obligations, String> {
        let channel = self.writer.channel();
        let posting = super::super::deliver::posts_in_flight(channel)
            .ok_or("the channel's POSTs in flight are not counted")?;
        let protected = self.verified.as_ref().map(super::super::owed::protected);
        Ok(Obligations {
            owed: self.owed.len(),
            prepared: usize::from(self.writer.store().ledger().unresolved().is_some()),
            unsealed: usize::from(self.deriver.has_unsealed()),
            posting,
            verified_work: self.verified.as_ref().map_or(0, |log| log.work.len()),
            protected: protected.unwrap_or_default().into_iter().collect(),
        })
    }
}

/// Settled only while the request's home holds the channel open here, or, for a channel without
/// a registered home, while the writer's gate owns it.
fn home_now(channel: u64, gate: &OwnershipGate, expected: Option<i64>) -> Home {
    match (channel_home::registered_channel(channel), expected) {
        (Some(home), Some(expected)) => match home.ownership() {
            HomeOwnership::Owned {
                home_epoch,
                intake: HomeIntake::Open,
                ..
            } if home_epoch == expected => Home::Settled,
            _ => Home::Moving,
        },
        (Some(_), None) => Home::Unread("the request names no home for a homed channel".into()),
        (None, Some(_)) => Home::Unread("the request's home is not registered here".into()),
        (None, None) if matches!(gate.current(), GatewayOwnership::Owned { .. }) => Home::Settled,
        (None, None) => Home::Moving,
    }
}

/// The pinned source and history of the pane's execution, read only while its spawn nonce is
/// still the one the request was checked against.
pub(crate) fn execution_read(channel: u64, context: &Context) -> Execution {
    #[cfg(test)]
    if let Some(forced) = FORCED_EXECUTION.with(|forced| forced.borrow().clone()) {
        return forced;
    }
    match observe_spawn_nonce_marker(&context.tmux) {
        SpawnNonceMarker::Known(nonce) if !nonce.is_empty() && nonce == context.nonce => {}
        _ => return Err("the pane's spawn nonce is not the request's".into()),
    }
    let history = binding_events::claude_history(channel, &context.tmux, Some(&context.nonce));
    history.map_err(|error| error.to_string())
}

/// The sources the execution holds by its history. A history not known whole, without a current
/// session, or with an unpinned session a candidate may be is an error, never an empty set.
pub(crate) fn execution_sources(
    history: &SourceHistory,
    candidates: &BTreeSet<&str>,
) -> Result<Vec<SourceId>, String> {
    if !history.complete {
        return Err("the execution's history is not complete".into());
    }
    let current = (history.current.as_ref()).ok_or("the execution has no current session")?;
    let visits = [Some(current), history.awaiting.as_ref()]
        .into_iter()
        .flatten();
    let visits = visits.map(|visit| (&visit.session, &visit.pin));
    let left = (history.left.iter()).map(|(session, left)| (session, &left.pin));
    let mut held = Vec::new();
    for (session, pin) in visits.chain(left) {
        match pin {
            Some(pin) => held.push(pin.clone()),
            None if candidates.contains(session.as_str()) || candidates.contains("") => {
                return Err(format!(
                    "session {session} of the execution has no pinned source"
                ));
            }
            None => {}
        }
    }
    Ok(held)
}

#[cfg(test)]
thread_local! {
    static FORCED_EXECUTION: std::cell::RefCell<Option<Execution>> = const { std::cell::RefCell::new(None) };
}

/// Answers every execution read on this thread with `read` until dropped.
#[cfg(test)]
pub(crate) fn force_execution_for_tests(read: Execution) -> impl Drop {
    struct Restore(Option<Execution>);
    impl Drop for Restore {
        fn drop(&mut self) {
            FORCED_EXECUTION.with(|forced| *forced.borrow_mut() = self.0.take());
        }
    }
    Restore(FORCED_EXECUTION.with(|forced| forced.borrow_mut().replace(read)))
}
