use super::super::native_row::{native_prompt, read_native_row};
use super::*;

pub(crate) const CAP: u64 = 16 * 1024 * 1024;

#[derive(Debug)]
pub(crate) struct Evidence {
    pub(crate) namespace: String,
    pub(crate) key: Value,
    pub(crate) source: Source,
    pub(crate) uuid: String,
    pub(crate) file: [u64; 2],
    pub(crate) range: (u64, u64),
    pub(crate) row_digest: String,
    pub(crate) frame_digest: String,
    snapshot: Value,
    class_hash: String,
    fingerprint: crate::services::tui_o::shadow::capture::Fingerprint,
}

#[derive(Debug)]
pub(crate) enum Consumed {
    User { row: Evidence },
    Command { row: Evidence },
}

pub(crate) fn lookup(
    primary: u64,
    namespace: &str,
    owner: &str,
    sid: &str,
    transcript: &Path,
    through: u64,
) -> Option<Consumed> {
    let state: Snapshot = serde_json::from_value(Registry::runtime().ok()?.json().ok()?).ok()?;
    let registrations: Vec<_> = state
        .live
        .iter()
        .map(|b| &b.registration)
        .chain(state.planned.iter())
        .chain(state.passive.iter().filter_map(|p| p.registration.as_ref()))
        .collect();
    let mut matching: Vec<&Registration> = Vec::new();
    for r in registrations
        .iter()
        .copied()
        .filter(|r| r.key["identity"]["primary"] == primary)
    {
        if !matching
            .iter()
            .any(|old| old.namespace == r.namespace && old.key == r.key)
        {
            matching.push(r);
        }
    }
    let [registration] = matching.as_slice() else {
        return None;
    };
    let nonce = registration.key["attempt"]["nonce"].as_str()?;
    if primary == 0
        || registration.namespace != namespace
        || nonce.is_empty()
        || registrations.iter().any(|r| {
            r.key["attempt"]["nonce"] == nonce
                && (r.namespace != namespace || r.key != registration.key)
        })
        || state.abandoned(namespace, &registration.key)
    {
        return None;
    }
    let (_, Some(entry)) = state.locate(namespace, &registration.key)? else {
        return None;
    };
    let source = checked_source(registration, entry, owner, sid, transcript)?;
    let fingerprint = source_fingerprint(transcript)?;
    let mut entries = Vec::new();
    for (r, e) in state.entries().filter(|(_, e)| {
        e.append.is_none()
            && e.provider == "claude"
            && e.session == owner
            && e.path == transcript
            && e.file[1] == source.file[1]
            && e.class_hash == entry.class_hash
    }) {
        let r = r?;
        if state.abandoned(&r.namespace, &r.key) {
            continue;
        }
        checked_source(r, e, owner, sid, transcript)?;
        let mut view = e.clone();
        view.class_hash = e.hash.clone();
        entries.push(view);
    }
    entries.sort_by_key(|e| e.offset);
    if entries
        .windows(2)
        .any(|pair| pair[0].offset == pair[1].offset)
    {
        return None;
    }
    let from = entries.first()?.offset;
    let entries: Vec<_> = entries.iter().collect();
    (through.checked_sub(from)? <= CAP).then_some(())?;
    let mut rows = Vec::new();
    let mut digests = std::collections::BTreeMap::new();
    let mut at = from;
    while at < through {
        let native = read_native_row(transcript, source.file, at, through - at, true)?;
        at = native.range.1;
        (at <= through).then_some(())?;
        let Some(text) = native_prompt(&native.record, sid) else {
            continue;
        };
        let mut taken = taken_record(&native.record, native.range.0)?;
        taken.hashes = native_frames(&text).map(hash).collect();
        if entries.iter().any(|e| taken.hashes.contains(&e.class_hash)) {
            if rows.iter().any(|r: &Taken| r.uuid == taken.uuid) {
                return None;
            }
            digests.insert(taken.offset, native.digest);
            rows.push(taken);
        }
    }
    let mut paired = Vec::new();
    for taken in &rows {
        let Some(owner) = pair(&entries, &rows, &taken.uuid) else {
            continue;
        };
        if owner.offset > taken.offset || !taken.hashes.contains(&owner.class_hash) {
            return None;
        }
        if owner.id == nonce {
            paired.push(taken);
        }
    }
    let [taken] = paired.as_slice() else {
        return None;
    };
    (taken.offset >= source.offset).then_some(())?;
    let native = read_native_row(
        transcript,
        source.file,
        taken.offset,
        through - taken.offset,
        true,
    )?;
    let text = native_prompt(&native.record, sid)?;
    let frame = native_frames(&text).find(|frame| hash(frame) == registration.frame.hash)?;
    if native.range.1 > through
        || native.digest != *digests.get(&taken.offset)?
        || native.record["uuid"] != taken.uuid
        || source_fingerprint(transcript)? != fingerprint
    {
        return None;
    }
    let row = Evidence {
        snapshot: selection(&state, &registration.key, &source, &entry.class_hash)?,
        class_hash: entry.class_hash.clone(),
        fingerprint,
        namespace: namespace.into(),
        key: registration.key.clone(),
        source,
        uuid: taken.uuid.clone(),
        file: native.file,
        range: native.range,
        row_digest: native.digest,
        frame_digest: blake3::hash(frame.as_bytes()).to_hex().to_string(),
    };
    Some(if taken.user {
        Consumed::User { row }
    } else {
        Consumed::Command { row }
    })
}

fn native_frames(text: &str) -> impl Iterator<Item = &str> {
    std::iter::once(text).chain(crate::services::claude_tui::busy_inject::pasted_inner(text))
}

fn checked_source(
    r: &Registration,
    e: &Entry,
    owner: &str,
    sid: &str,
    path: &Path,
) -> Option<Source> {
    let source = r.source.as_ref()?;
    if source.provider != "claude"
        || source.target["host"] != "tmux"
        || owner.is_empty()
        || source.target["owner"] != owner
        || source.session_id != sid
        || sid.is_empty()
        || source.transcript != path
        || e.id != r.key["attempt"]["nonce"].as_str()?
        || e.provider != source.provider
        || e.session != owner
        || e.path != path
        || e.file != source.file
        || e.offset != source.offset
        || e.body_hash != r.frame.body_hash
        || e.hash != r.frame.hash
        || e.class_hash != r.frame.class_hash
        || e.append.is_some()
    {
        return None;
    }
    let mut view = source.clone();
    view.file[0] = file_id(&std::fs::symlink_metadata(path).ok()?)[0];
    let mut key = r.key.clone();
    if key["attempt"]["source"]["kind"] == "existing" {
        (key["attempt"]["source"]["file"] == json!(source.file)).then_some(())?;
        key["attempt"]["source"]["file"] = json!(view.file);
    }
    view.validate(&key).ok()?;
    Some(source.clone())
}

#[cfg(test)]
#[path = "carried_tests.rs"]
mod tests;

fn source_fingerprint(path: &Path) -> Option<crate::services::tui_o::shadow::capture::Fingerprint> {
    crate::services::tui_o::shadow::capture::Fingerprint::of(&std::fs::symlink_metadata(path).ok()?)
        .ok()
}

fn selection(state: &Snapshot, key: &Value, source: &Source, class: &str) -> Option<Value> {
    let relevant = |r: Option<&Registration>, e: Option<&Entry>| {
        r.is_some_and(|r| {
            r.key["identity"]["primary"] == key["identity"]["primary"]
                || r.key["attempt"]["nonce"] == key["attempt"]["nonce"]
        }) || e.is_some_and(|e| {
            e.append.is_none()
                && e.provider == "claude"
                && source.target["owner"] == e.session
                && e.path == source.transcript
                && e.file[1] == source.file[1]
                && e.class_hash == class
        })
    };
    let live: Vec<_> = state
        .live
        .iter()
        .filter(|b| relevant(Some(&b.registration), Some(&b.entry)))
        .collect();
    let planned: Vec<_> = state
        .planned
        .iter()
        .filter(|r| relevant(Some(r), None))
        .collect();
    let passive: Vec<_> = state
        .passive
        .iter()
        .filter(|p| relevant(p.registration.as_ref(), p.entry.as_ref()))
        .collect();
    serde_json::to_value((live, planned, passive)).ok()
}

impl Consumed {
    pub(crate) fn evidence(&self) -> &Evidence {
        match self {
            Self::User { row } | Self::Command { row } => row,
        }
    }

    /// Publish only while the registry still has the exact selection and competing owners.
    pub(crate) fn confirm<T>(&self, publish: impl FnOnce() -> Option<T>) -> Option<T> {
        let row = self.evidence();
        Registry::runtime()
            .ok()?
            .inspect(|state| {
                if selection(state, &row.key, &row.source, &row.class_hash).as_ref()
                    != Some(&row.snapshot)
                    || state.locate(&row.namespace, &row.key).is_none()
                {
                    return Ok(None);
                }
                let verify = || -> Option<()> {
                    let native = read_native_row(
                        &row.source.transcript,
                        row.source.file,
                        row.range.0,
                        row.range.1.checked_sub(row.range.0)?,
                        true,
                    )?;
                    if native.file != row.file
                        || native.range != row.range
                        || native.digest != row.row_digest
                        || native.record["uuid"] != row.uuid
                        || source_fingerprint(&row.source.transcript)? != row.fingerprint
                        || (native.record["type"] == "user") != matches!(self, Self::User { .. })
                    {
                        return None;
                    }
                    native_prompt(&native.record, &row.source.session_id)?;
                    Some(())
                };
                Ok(verify().and_then(|()| publish()))
            })
            .ok()
            .flatten()
    }
}
