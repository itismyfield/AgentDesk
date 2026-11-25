//! The registry's decoded history is reused without reading while the file keeps the stamp its
//! bytes were read under. Any mutable snapshot drops that stamp and every view derived from it.
use super::*;
use crate::services::tui_o::shadow::capture::Fingerprint;
use std::io::Read;
use std::time::{Duration, SystemTime};

/// A later write lands at least this far past a settled time, so no clock tick repeats a stamp.
const SETTLED: Duration = Duration::from_secs(2);

#[cfg(unix)]
type View = Vec<Option<Result<native::EvidenceRegistration, String>>>;

struct Kept {
    raw: String,
    state: Snapshot,
    /// The read descriptor's stamp, kept only once both of its times had settled.
    stamp: Option<Fingerprint>,
    /// Evidence and classification results per registration, in query order.
    #[cfg(unix)]
    views: [View; 2],
}

impl Kept {
    fn forget(&mut self) {
        self.stamp = None;
        #[cfg(unix)]
        self.views.iter_mut().for_each(Vec::clear);
    }
}

#[derive(Default)]
pub(super) struct Cache {
    kept: Option<Kept>,
    /// History reads and registration results computed through this cache, for the cost tests.
    #[cfg(test)]
    pub(super) reads: usize,
    #[cfg(test)]
    pub(super) derived: usize,
}

impl Cache {
    pub(super) fn raw(&self) -> Option<&str> {
        self.kept.as_ref().map(|kept| kept.raw.as_str())
    }

    pub(super) fn state(&self) -> Option<&Snapshot> {
        self.kept.as_ref().map(|kept| &kept.state)
    }

    /// The only way to a mutable snapshot: the old one's stamp and views go with it.
    pub(super) fn state_mut(&mut self) -> Option<&mut Snapshot> {
        let kept = self.kept.as_mut()?;
        kept.forget();
        Some(&mut kept.state)
    }

    pub(super) fn replace(&mut self, raw: String, state: Snapshot) {
        self.kept = Some(Kept {
            raw,
            state,
            stamp: None,
            #[cfg(unix)]
            views: Default::default(),
        });
    }

    pub(super) fn clear(&mut self) {
        self.kept = None;
    }

    /// Names the bytes a transaction just published from the snapshot it changed.
    pub(super) fn published(&mut self, raw: String) -> Result<(), String> {
        let kept = self.kept.as_mut().ok_or("history cache disappeared")?;
        kept.raw = raw;
        Ok(())
    }

    /// Holds the file's current V2 history afterwards, reading it unless the kept stamp still
    /// names the path. The caller holds the history lock; `stat` stamps the file it was read from.
    pub(super) fn refresh(
        &mut self,
        path: &Path,
        now: SystemTime,
        stat: impl FnMut(&std::fs::File) -> std::io::Result<Fingerprint>,
    ) -> Result<(), String> {
        let stamp = self.kept.as_ref().and_then(|kept| kept.stamp.as_ref());
        if let Some(stamp) = stamp
            && std::fs::metadata(path)
                .is_ok_and(|meta| Fingerprint::of(&meta).ok().as_ref() == Some(stamp))
        {
            return Ok(());
        }
        #[cfg(test)]
        {
            self.reads += 1;
        }
        let (raw, stamp) = read(path, now, stat).map_err(|e| e.to_string())?;
        if self.raw() != Some(raw.as_str()) {
            let migration::Decoded::V2(snapshot) = migration::decode(&raw)? else {
                return Err("V2 registration missing".into());
            };
            self.replace(raw, snapshot);
        }
        if let Some(kept) = &mut self.kept {
            kept.stamp = stamp;
        }
        Ok(())
    }
}

/// The stamp of an open history file.
pub(super) fn fstat(file: &std::fs::File) -> std::io::Result<Fingerprint> {
    Fingerprint::of(&file.metadata()?)
}

/// The file's bytes and the stamp of the descriptor they came from: no stamp when it could not be
/// taken or changed during the read, or while a time is too fresh to tell a rewrite apart.
fn read(
    path: &Path,
    now: SystemTime,
    mut stat: impl FnMut(&std::fs::File) -> std::io::Result<Fingerprint>,
) -> std::io::Result<(String, Option<Fingerprint>)> {
    let mut file = std::fs::File::open(path)?;
    let before = stat(&file).ok();
    let mut raw = String::new();
    file.read_to_string(&mut raw)?;
    let after = stat(&file).ok();
    let stamp = after.filter(|after| before.as_ref() == Some(after) && settled(after, now));
    Ok((raw, stamp))
}

/// Only Unix stamps carry the inode and change time that tell a rewrite with a restored mtime
/// apart, so other platforms always read.
fn settled(stamp: &Fingerprint, now: SystemTime) -> bool {
    let change = u64::try_from(stamp.change.0)
        .ok()
        .zip(u32::try_from(stamp.change.1).ok())
        .and_then(|(secs, nanos)| SystemTime::UNIX_EPOCH.checked_add(Duration::new(secs, nanos)));
    cfg!(unix)
        && [Some(stamp.modified), change].into_iter().all(|at| {
            at.and_then(|at| at.checked_add(SETTLED))
                .is_some_and(|at| at <= now)
        })
}

#[cfg(unix)]
type Item<'a> = (&'a Registration, Option<&'a Entry>, &'a native::Settlement);

#[cfg(unix)]
fn evidence_items(state: &Snapshot, classify: bool) -> impl Iterator<Item = Item<'_>> {
    let registrations = state
        .live
        .iter()
        .map(|b| (&b.registration, Some(&b.entry)))
        .chain(state.planned.iter().map(|r| (r, None)))
        .chain(
            state
                .passive
                .iter()
                .filter(|p| p.state != collision::ABANDONED)
                .filter_map(|p| p.registration.as_ref().map(|r| (r, p.entry.as_ref()))),
        );
    registrations.filter_map(move |(registration, entry)| {
        registration
            .native_settlement
            .as_ref()
            .filter(|s| classify || (s.outcome.is_none() && !s.cleanup_complete))
            .map(|s| (registration, entry, s))
    })
}

#[cfg(unix)]
fn evidence_item(
    (registration, entry, settlement): Item<'_>,
) -> Result<native::EvidenceRegistration, String> {
    settlement.validate(registration)?;
    let open = settlement.outcome.is_none() && !settlement.cleanup_complete;
    Ok(native::EvidenceRegistration {
        namespace: registration.namespace.clone(),
        candidate: open && !settlement.extra.contains_key("foreign_prompt_barrier"),
        barrier: match open {
            true => settlement.known_barrier(&registration.namespace)?,
            false => None,
        },
        ticket: settlement.ticket.clone(),
        frame_hash: registration.frame.class_hash.clone(),
        exact_frame_hash: registration.frame.hash.clone(),
        receipts: entry
            .into_iter()
            .flat_map(|e| e.events.iter())
            .filter(|e| matches!(e.kind.as_str(), "user" | "command"))
            .filter_map(|e| e.row.clone())
            .collect(),
    })
}

#[cfg(unix)]
pub(super) fn evidence_registrations(
    state: &Snapshot,
    classify: bool,
) -> Result<Vec<native::EvidenceRegistration>, String> {
    evidence_items(state, classify).map(evidence_item).collect()
}

#[cfg(unix)]
impl Cache {
    /// `evidence_registrations`, keeping each result that the snapshot alone decides. A stored
    /// barrier stats its transcript, so that registration is validated again on every call.
    pub(super) fn evidence(
        &mut self,
        classify: bool,
    ) -> Result<Vec<native::EvidenceRegistration>, String> {
        let kept = self.kept.as_mut().ok_or("history cache missing")?;
        let view = &mut kept.views[usize::from(classify)];
        let mut registrations = Vec::new();
        for (at, item) in evidence_items(&kept.state, classify).enumerate() {
            let stored = item.2.extra.contains_key("foreign_prompt_barrier");
            if view.len() <= at {
                view.push(None);
            }
            let result = match &view[at] {
                Some(known) if !stored => known.clone(),
                _ => {
                    #[cfg(test)]
                    {
                        self.derived += 1;
                    }
                    let fresh = evidence_item(item);
                    if !stored {
                        view[at] = Some(fresh.clone());
                    }
                    fresh
                }
            };
            registrations.push(result?);
        }
        Ok(registrations)
    }
}

#[cfg(all(test, unix))]
#[path = "read_stamp_tests.rs"]
mod tests;
