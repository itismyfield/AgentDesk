//! The registry's decoded history is reused without reading while the file keeps the stamp its
//! bytes were read under. A mutable snapshot drops its derived views before publication.
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
    legacy: bool,
    #[cfg(unix)]
    revision: u64,
    /// The read descriptor's stamp, kept only once both of its times had settled.
    stamp: Option<Fingerprint>,
    /// Evidence and classification results per registration, in query order.
    #[cfg(unix)]
    views: [View; 2],
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

    /// A mutation invalidates derived views. Its raw bytes keep their stamp until publication.
    pub(super) fn state_mut(&mut self) -> Option<&mut Snapshot> {
        let kept = self.kept.as_mut()?;
        #[cfg(unix)]
        kept.views.iter_mut().for_each(Vec::clear);
        Some(&mut kept.state)
    }

    fn replace(&mut self, raw: String, decoded: migration::Decoded) {
        let legacy = matches!(decoded, migration::Decoded::Legacy(_));
        self.kept = Some(Kept {
            raw,
            state: decoded.into_snapshot(),
            legacy,
            #[cfg(unix)]
            revision: next_revision(),
            stamp: None,
            #[cfg(unix)]
            views: Default::default(),
        });
    }

    pub(super) fn clear(&mut self) {
        self.kept = None;
    }

    pub(super) fn take_raw(&mut self) -> Option<String> {
        self.kept.take().map(|kept| kept.raw)
    }

    pub(super) fn legacy(&self) -> bool {
        self.kept.as_ref().is_some_and(|kept| kept.legacy)
    }

    /// Names the bytes a transaction just published from the snapshot it changed.
    pub(super) fn published(&mut self, raw: String) -> Result<(), String> {
        let kept = self.kept.as_mut().ok_or("history cache disappeared")?;
        kept.raw = raw;
        kept.stamp = None;
        kept.legacy = false;
        #[cfg(unix)]
        {
            kept.revision = next_revision();
        }
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
        self.load(path, now, stat, false, |_| Ok(()))?;
        if self.legacy() {
            return Err("V2 registration missing".into());
        }
        Ok(())
    }

    fn load(
        &mut self,
        path: &Path,
        now: SystemTime,
        stat: impl FnMut(&std::fs::File) -> std::io::Result<Fingerprint>,
        missing_is_empty: bool,
        check_raw: impl FnOnce(&str) -> Result<(), String>,
    ) -> Result<bool, String> {
        let stamp = self.kept.as_ref().and_then(|kept| kept.stamp.as_ref());
        if let Some(stamp) = stamp
            && std::fs::metadata(path)
                .is_ok_and(|meta| Fingerprint::of(&meta).ok().as_ref() == Some(stamp))
        {
            check_raw(self.raw().ok_or("history cache missing")?)?;
            return Ok(true);
        }
        #[cfg(test)]
        {
            self.reads += 1;
        }
        let (raw, stamp, present) = match read(path, now, stat) {
            Ok((raw, stamp)) => (raw, stamp, true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && missing_is_empty => {
                (migration::MISSING.to_owned(), None, false)
            }
            Err(e) => return Err(e.to_string()),
        };
        check_raw(&raw)?;
        if self.raw() != Some(raw.as_str()) {
            let decoded = migration::decode(&raw)?;
            self.replace(raw, decoded);
        }
        if let Some(kept) = &mut self.kept {
            kept.stamp = stamp;
        }
        Ok(present)
    }
}

impl Registry {
    pub(super) fn refresh_transaction_cache(
        &self,
        cache: &mut Cache,
        pending: &mut PendingPublication,
        unconfirmed: &str,
    ) -> Result<bool, String> {
        use crate::services::discord::runtime_store::{
            AtomicWriteContext,
            durable_publish::{FlushStatus, retry_published_status},
        };
        cache.load(&self.path, SystemTime::now(), fstat, true, |raw| {
            if let Some((old, candidate)) = pending.as_ref() {
                if raw == candidate {
                    let context = AtomicWriteContext::new("injected_frames_v2");
                    if retry_published_status(&self.path, context) != FlushStatus::Flushed {
                        return Err(unconfirmed.into());
                    }
                } else if raw != old {
                    return Err(
                        "history changed after an unconfirmed publication; preserve both".into(),
                    );
                }
                *pending = None;
            }
            Ok(())
        })
    }
}

#[cfg(unix)]
fn next_revision() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
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
fn settlement_items(state: &Snapshot, include_abandoned: bool) -> impl Iterator<Item = Item<'_>> {
    let registrations = state
        .live
        .iter()
        .map(|b| (&b.registration, Some(&b.entry)))
        .chain(state.planned.iter().map(|r| (r, None)))
        .chain(
            state
                .passive
                .iter()
                .filter(move |p| include_abandoned || p.state != collision::ABANDONED)
                .filter_map(|p| p.registration.as_ref().map(|r| (r, p.entry.as_ref()))),
        );
    registrations.filter_map(|(registration, entry)| {
        registration
            .native_settlement
            .as_ref()
            .map(|s| (registration, entry, s))
    })
}

#[cfg(unix)]
fn evidence_items(state: &Snapshot, classify: bool) -> impl Iterator<Item = Item<'_>> {
    settlement_items(state, false).filter(move |(_, _, s)| classify || evidence_open(s))
}

#[cfg(unix)]
fn evidence_open(settlement: &native::Settlement) -> bool {
    settlement.outcome.is_none() && !settlement.cleanup_complete && !settlement.not_sent()
}

#[cfg(unix)]
fn evidence_item(
    (registration, entry, settlement): Item<'_>,
) -> Result<native::EvidenceRegistration, String> {
    settlement.validate(registration)?;
    let open = evidence_open(settlement);
    Ok(native::EvidenceRegistration {
        namespace: registration.namespace.clone(),
        candidate: open && !settlement.extra.contains_key("foreign_prompt_barrier"),
        not_sent: settlement.not_sent(),
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
    pub(super) fn revision(&self) -> Result<u64, String> {
        self.kept
            .as_ref()
            .map(|kept| kept.revision)
            .ok_or("history cache missing".into())
    }

    pub(super) fn has_open_barrier(&self, channel: u64) -> Result<bool, String> {
        let state = self.state().ok_or("history cache missing")?;
        Ok(settlement_items(state, true).any(|(_, _, settlement)| {
            settlement.ticket.channel_id == channel
                && settlement.outcome.is_none()
                && settlement.extra.contains_key("foreign_prompt_barrier")
        }))
    }

    /// `evidence_registrations`, keeping each result that the snapshot alone decides. A stored
    /// barrier stats its transcript, so that registration is validated again on every call.
    pub(super) fn evidence(
        &mut self,
        classify: bool,
        channel: Option<u64>,
    ) -> Result<Vec<native::EvidenceRegistration>, String> {
        let kept = self.kept.as_mut().ok_or("history cache missing")?;
        let view = &mut kept.views[usize::from(classify)];
        let mut registrations = Vec::new();
        for (at, item) in evidence_items(&kept.state, classify).enumerate() {
            let stored = item.2.extra.contains_key("foreign_prompt_barrier");
            if channel.is_some_and(|channel| channel != item.2.ticket.channel_id) {
                // Preserve the whole-view validation without cloning other channels' rows.
                match view.get(at).and_then(Option::as_ref) {
                    Some(known) if !stored => {
                        known.as_ref().map_err(Clone::clone)?;
                    }
                    _ => item.2.validate(item.0)?,
                }
                continue;
            }
            if view.len() <= at {
                view.resize_with(at + 1, || None);
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
