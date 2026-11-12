//! Completion tombstones outlive the activity lease; replay never refreshes a Pre.
use super::transcript::{Frame, Turn};
use chrono::{DateTime, Utc};
use std::collections::HashMap;

pub(super) const TOOL_LEASE: chrono::Duration = chrono::Duration::seconds(30);
const PAIR_LIMIT: usize = 4096;
#[derive(Default, Clone, PartialEq, Eq)]
struct Pair {
    pre: Option<DateTime<Utc>>,
    complete: bool,
}
#[derive(Clone, PartialEq, Eq)]
struct Unassigned {
    id: String,
    at: DateTime<Utc>,
    post: bool,
    prompt: Option<String>,
}
#[derive(Default, Clone, PartialEq, Eq)]
pub(super) struct Tools {
    pairs: HashMap<(Turn, String), Pair>,
    unassigned: Vec<Unassigned>,
    overflow: bool,
}
impl Tools {
    pub(super) fn uncertain(&self) -> bool {
        self.overflow
    }
    pub(super) fn note(&mut self, turn: &Turn, id: &str, at: DateTime<Utc>, post: bool) {
        let key = (turn.clone(), id.to_owned());
        if self.pairs.len() + self.unassigned.len() >= PAIR_LIMIT && !self.pairs.contains_key(&key)
        {
            self.overflow = true;
            return;
        }
        let pair = self.pairs.entry(key).or_default();
        if post {
            pair.complete = true;
        } else if !pair.complete {
            pair.pre = Some(pair.pre.map_or(at, |old| old.min(at)));
        }
    }
    pub(super) fn record(
        &mut self,
        frame: &Frame,
        id: &str,
        at: DateTime<Utc>,
        post: bool,
        prompt: Option<&str>,
    ) {
        if frame.caught_up()
            && frame.uses.contains_key(id)
            && let Some(turn) = &frame.turn
            && at >= turn.at
            && prompt.is_none_or(|prompt| Some(prompt) == turn.prompt.as_deref())
        {
            self.note(turn, id, at, post);
            return;
        }
        let event = Unassigned {
            id: id.into(),
            at,
            post,
            prompt: prompt.map(str::to_owned),
        };
        if self.unassigned.contains(&event) {
            return;
        }
        if self.pairs.len() + self.unassigned.len() >= PAIR_LIMIT {
            self.overflow = true;
            return;
        }
        self.unassigned.push(event);
    }
    pub(super) fn reconcile(&mut self, frame: &Frame) {
        if frame.caught_up()
            && let Some(turn) = &frame.turn
        {
            let pending = std::mem::take(&mut self.unassigned);
            for event in pending {
                if event.at < turn.at {
                    continue;
                }
                if frame.uses.contains_key(&event.id)
                    && event
                        .prompt
                        .as_deref()
                        .is_none_or(|prompt| Some(prompt) == turn.prompt.as_deref())
                {
                    self.note(turn, &event.id, event.at, event.post);
                } else {
                    self.unassigned.push(event);
                }
            }
        }
        for ((turn, id), pair) in &mut self.pairs {
            if frame.turn.as_ref() == Some(turn)
                && (frame.terminal > turn.floor || frame.results.contains(id))
            {
                pair.complete = true;
            }
        }
        // A current terminal is a high-water mark, not a timeout-based tombstone eviction.
        if frame.terminal > 0 {
            self.pairs
                .retain(|(turn, _), _| turn.floor >= frame.terminal);
            self.overflow = false;
        }
    }
    pub(super) fn lease_end(&self, frame: &Frame, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        if self.overflow {
            return None;
        }
        let turn = frame.turn.as_ref()?;
        frame
            .uses
            .iter()
            .filter(|(id, invocation)| !invocation.background && !frame.results.contains(*id))
            .filter_map(|(id, _)| {
                let pair = self.pairs.get(&(turn.clone(), id.clone()))?;
                let at = pair.pre?;
                (!pair.complete && now >= at && now - at < TOOL_LEASE).then_some(at + TOOL_LEASE)
            })
            .max()
    }
    pub(super) fn busy(&self, frame: &Frame, now: DateTime<Utc>) -> bool {
        self.lease_end(frame, now).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::super::{source::Stamp, transcript::Invocation};
    use super::*;
    use std::{collections::HashSet, time::SystemTime};
    fn frame() -> Frame {
        Frame {
            stamp: Stamp {
                len: 200,
                modified: SystemTime::UNIX_EPOCH,
            },
            turn: Some(Turn {
                floor: 10,
                prompt: Some("root-a".into()),
                at: DateTime::from_timestamp(100, 0).unwrap(),
            }),
            uses: HashMap::from([("same-id".into(), Invocation { background: false })]),
            results: HashSet::new(),
            terminal: 0,
            progress: None,
            through: 200,
            partial: Vec::new(),
        }
    }
    #[test]
    fn pre_post_orders_duplicates_and_thirty_second_boundary() {
        let frame = frame();
        let turn = frame.turn.as_ref().unwrap();
        let at = turn.at;
        let mut pairs = Tools::default();
        pairs.note(turn, "same-id", at, false);
        assert!(pairs.busy(&frame, at + TOOL_LEASE - chrono::Duration::nanoseconds(1)));
        assert!(!pairs.busy(&frame, at + TOOL_LEASE));
        assert!(!pairs.busy(&frame, at + TOOL_LEASE + chrono::Duration::nanoseconds(1)));
        pairs.note(turn, "same-id", at + TOOL_LEASE, false);
        assert!(
            !pairs.busy(&frame, at + TOOL_LEASE),
            "duplicate Pre cannot renew its lease"
        );
        pairs.note(turn, "same-id", at + TOOL_LEASE, true);
        assert!(!pairs.busy(&frame, at));
        let mut reversed = Tools::default();
        reversed.note(turn, "same-id", at, true);
        reversed.note(turn, "same-id", at, false);
        assert!(!reversed.busy(&frame, at), "Post-first must remain closed");
        reversed.note(turn, "same-id", at + chrono::Duration::hours(1), false);
        assert!(
            !reversed.busy(&frame, at + chrono::Duration::hours(1)),
            "late replay cannot reopen a tombstone"
        );
    }
    #[test]
    fn lost_hooks_exact_result_and_terminal_high_water_recover() {
        let mut frame = frame();
        let turn = frame.turn.clone().unwrap();
        let at = turn.at;
        let mut pairs = Tools::default();
        assert!(
            !pairs.busy(&frame, at),
            "restart/lost Pre cannot invent a fresh lease"
        );
        pairs.note(&turn, "same-id", at, false);
        pairs.note(&turn, "other-id", at, true);
        assert!(pairs.busy(&frame, at));
        frame.results.insert("other-id".into());
        pairs.reconcile(&frame);
        assert!(pairs.busy(&frame, at));
        frame.results.insert("same-id".into());
        pairs.reconcile(&frame);
        frame.results.clear();
        assert!(
            !pairs.busy(&frame, at),
            "a transcript result closes the lost Post"
        );
        let mut pairs = Tools::default();
        pairs.note(&turn, "same-id", at, false);
        frame.terminal = 100;
        pairs.reconcile(&frame);
        assert!(!pairs.busy(&frame, at));
    }
    #[test]
    fn root_turn_and_exact_invocation_are_required() {
        let mut frame = frame();
        let turn = frame.turn.clone().unwrap();
        let at = turn.at;
        let mut pairs = Tools::default();
        let mut other = turn.clone();
        other.floor = 11;
        pairs.note(&other, "same-id", at, false);
        assert!(!pairs.busy(&frame, at));
        pairs.note(&turn, "foreign-id", at, false);
        assert!(
            !pairs.busy(&frame, at),
            "missing child field is not a root witness"
        );
        pairs.note(&turn, "same-id", at, false);
        assert!(pairs.busy(&frame, at));
        frame.uses.get_mut("same-id").unwrap().background = true;
        assert!(
            !pairs.busy(&frame, at),
            "Agent/Task ambiguity cannot become foreground"
        );
    }
    #[test]
    fn storage_pressure_expires_authority_without_dropping_tombstones() {
        let frame = frame();
        let turn = frame.turn.as_ref().unwrap();
        let at = turn.at;
        let mut pairs = Tools::default();
        pairs.note(turn, "same-id", at, true);
        for n in 0..PAIR_LIMIT {
            pairs.note(turn, &format!("other-{n}"), at, false);
        }
        pairs.note(turn, "same-id", at, false);
        assert!(!pairs.busy(&frame, at));
        assert!(pairs.pairs[&(turn.clone(), "same-id".into())].complete);
        let mut empty_frame = frame.clone();
        empty_frame.uses.clear();
        let entry = super::super::Entry {
            tools: pairs,
            ..Default::default()
        };
        assert_eq!(
            super::super::foreground(&entry, &empty_frame, at),
            super::super::Foreground::Unknown
        );
    }
}
