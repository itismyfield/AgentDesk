//! Which registered inputs each native run holds, read off the evidence reduction's own
//! ticket-to-run membership so reply anchors bind exactly where receipts and evidence do.

use super::*;
use crate::services::tui_o::writer::reply_anchor::{RunAnchor, anchor_of};

impl Projection {
    /// Every run so far with its opener and joined anchors, as the reduction's uniqueness rules
    /// place them; a member needs exact evidence, and a repeated body binds nothing.
    pub(crate) fn memberships(
        &self,
        registrations: &[Registration],
    ) -> Result<Vec<RunAnchor>, String> {
        let mut scoped = registrations.to_vec();
        for registration in &mut scoped {
            registration.candidate = true;
            registration.barrier = None;
        }
        let receipts: Vec<_> = self
            .facts
            .iter()
            .filter_map(|f| f.receipt.as_ref())
            .collect();
        let mut members: Vec<(String, Ticket, Option<u64>)> = Vec::new();
        // A membership without a row id is evidence only when its record is the absorption.
        let absorbed = |range: (u64, u64)| {
            let absorption = |f: &Fact| matches!(f.event, Some(Event::Absorbed(_)));
            self.facts.iter().any(|f| f.range == range && absorption(f))
        };
        reduce(
            &self.binding,
            (&self.facts, None),
            &scoped,
            self.through,
            Some(&mut |registration, range, row| {
                let known = members.iter().any(|(namespace, ticket, _)| {
                    *namespace == registration.namespace && *ticket == registration.ticket
                });
                // Only exact evidence names an anchor; a trimmed class-hash match is a guess.
                let evidence = row.is_some() || absorbed(range);
                if !known && evidence && super::super::receipt::exact(&receipts, registration, row)
                {
                    // The same exact body twice after the offset is no single row.
                    let at = (!super::super::receipt::repeated(&receipts, registration))
                        .then_some(range.0);
                    let key = (registration.namespace.clone(), registration.ticket.clone());
                    members.push((key.0, key.1, at));
                }
            }),
            None,
        )?;
        let mut runs: Vec<RunAnchor> = Vec::new();
        for edge in self.facts.iter().flat_map(|fact| &fact.boundaries) {
            match edge {
                Edge::Opened { range, .. } => runs.push(RunAnchor::opened(range.0)),
                Edge::Closed { range, .. } => {
                    if let Some(run) = runs.last_mut().filter(|run| run.end.is_none()) {
                        run.end = Some(range.1);
                    }
                }
            }
        }
        let mut placed: Vec<(usize, bool, u64)> = Vec::new();
        for (_, ticket, at) in &members {
            let (Some(at), Some(anchor)) = (at, anchor_of(ticket)) else {
                continue;
            };
            // A record that opens a run is its opener; a later record inside an open run joined it.
            let place = match runs.iter().position(|run| run.start == *at) {
                Some(index) => Some((index, true)),
                None => match runs
                    .iter()
                    .rposition(|run| run.start < *at && run.end.is_none_or(|end| *at < end))
                {
                    Some(index) => Some((index, false)),
                    None => runs
                        .iter()
                        .position(|run| run.start > *at)
                        .map(|i| (i, true)),
                },
            };
            if let Some((index, opener)) = place {
                placed.push((index, opener, anchor));
            }
        }
        for (index, opener, anchor) in placed {
            let run = &mut runs[index];
            if opener && run.opener.is_none() && !run.contested {
                run.opener = Some(anchor);
            } else if opener {
                // Two openers for one run leave it without one rather than guess.
                run.contested = true;
                run.opener = None;
            } else if !run.joined.contains(&anchor) {
                run.joined.push(anchor);
            }
        }
        Ok(runs)
    }
}
