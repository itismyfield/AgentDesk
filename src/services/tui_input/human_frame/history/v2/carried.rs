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
        entries.push(e);
    }
    entries.sort_by_key(|e| e.offset);
    if entries
        .windows(2)
        .any(|pair| pair[0].offset == pair[1].offset)
    {
        return None;
    }
    let from = entries.first()?.offset;
    (through.checked_sub(from)? <= CAP).then_some(())?;
    let mut rows = Vec::new();
    let mut digests = std::collections::BTreeMap::new();
    let mut at = from;
    while at < through {
        let native = read_native_row(transcript, source.file, at, through - at, true)?;
        at = native.range.1;
        (at <= through).then_some(())?;
        if native_prompt(&native.record, sid).is_none() {
            continue;
        }
        let taken = taken_record(&native.record, native.range.0)?;
        if taken.hashes.contains(&entry.class_hash) {
            if rows.iter().any(|r: &Taken| r.uuid == taken.uuid) {
                return None;
            }
            digests.insert(taken.offset, native.digest);
            rows.push(taken);
        }
    }
    let paired: Vec<_> = rows
        .iter()
        .filter(|r| pair(&entries, &rows, &r.uuid).is_some_and(|e| std::ptr::eq(e, entry)))
        .collect();
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
    let frame = if hash(&text) == registration.frame.hash {
        text.as_str()
    } else {
        crate::services::claude_tui::busy_inject::pasted_inner(&text)?
    };
    if native.range.1 > through
        || native.digest != *digests.get(&taken.offset)?
        || native.record["uuid"] != taken.uuid
        || hash(frame) != registration.frame.hash
    {
        return None;
    }
    let row = Evidence {
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
