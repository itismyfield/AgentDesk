//! Codex append receipts extend the existing frame entry without changing exact-frame readers.
use super::*;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Admission {
    #[default]
    Reserved,
    EffectPossible,
    Uncertain,
    Admitted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Decision {
    Joined,
    HandedPending,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Incarnation {
    generation: Option<([u64; 2], u64, u32)>,
    nonce: Option<String>,
    session: Option<String>,
}

fn incarnation(session: &str, transcript: &Path) -> Incarnation {
    let temp = crate::services::tmux_common::resolve_session_temp_path;
    let generation = temp(session, "generation").and_then(|p| {
        let meta = std::fs::metadata(p).ok()?;
        let modified = meta
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?;
        Some((file_id(&meta), modified.as_secs(), modified.subsec_nanos()))
    });
    let nonce = temp(session, "spawn_nonce")
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|nonce| nonce.trim().to_string());
    let session = crate::services::codex_tui::rollout_index::read_rollout_session_meta(transcript)
        .and_then(|meta| meta.id);
    Incarnation {
        generation,
        nonce,
        session,
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Bound {
    row: String,
    start: u64,
    prompt_hash: String,
    turn: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct State {
    frame_bytes: usize,
    separator: bool,
    target_turn: String,
    message_id: Option<u64>,
    incarnation: Incarnation,
    pub(super) admission: Admission,
    receipt: Option<Bound>,
    decision: Option<Decision>,
}

#[derive(Clone, Debug)]
pub(crate) struct Projection {
    pub id: String,
    pub target_turn: String,
    pub message_id: Option<u64>,
    pub admitted: bool,
    pub bound: bool,
    pub row: Option<String>,
    pub decision: Option<Decision>,
}

pub(crate) struct Receipt {
    pub session: String,
    pub attempt: Projection,
    pub prompt: String,
    pub row: String,
    pub native_turn: Option<String>,
}

fn projected(entry: &Entry) -> Projection {
    let a = entry.append.as_ref().expect("append projection");
    Projection {
        id: entry.id.clone(),
        target_turn: a.target_turn.clone(),
        message_id: a.message_id,
        admitted: a.admission == Admission::Admitted,
        bound: a.receipt.is_some(),
        row: a.receipt.as_ref().map(|r| r.row.clone()),
        decision: a.decision,
    }
}

pub(super) fn unbound(entry: &Entry) -> bool {
    entry.append.as_ref().map_or_else(
        || entry.events.is_empty(),
        |a| a.receipt.is_none() && current(entry),
    )
}

fn current(entry: &Entry) -> bool {
    let Some(a) = &entry.append else { return false };
    std::fs::metadata(&entry.path).is_ok_and(|meta| {
        file_id(&meta) == entry.file
            && meta.len() >= entry.offset
            && incarnation(&entry.session, &entry.path) == a.incarnation
    })
}

pub(crate) fn register(
    (session, id, author): (&str, &str, &str),
    transcript: &Path,
    offset: u64,
    frame: &str,
    target_turn: &str,
    message_id: Option<u64>,
    separator: bool,
) -> Result<(), String> {
    #[cfg(test)]
    if let Some(result) =
        super::v2::scoped_register(("codex", session, id, author), transcript, offset, frame)
    {
        return result;
    }
    if target_turn.trim().is_empty()
        || frame.is_empty()
        || std::fs::metadata(transcript)
            .map_err(|e| e.to_string())?
            .len()
            < offset
    {
        return Err("invalid append registration".into());
    }
    super::register_inner(
        ("codex", session, id, author),
        transcript,
        offset,
        frame,
        Some(State {
            frame_bytes: frame.len(),
            separator,
            target_turn: target_turn.into(),
            message_id,
            incarnation: incarnation(session, transcript),
            admission: Admission::Reserved,
            receipt: None,
            decision: None,
        }),
    )
}

pub(crate) fn mark(id: &str, transcript: &Path, admission: Admission) -> Result<(), String> {
    #[cfg(test)]
    if super::v2::scoped() {
        return Ok(());
    }
    update(|store| {
        let entry = store
            .entries
            .iter_mut()
            .find(|e| e.id == id && e.path == transcript && current(e))
            .ok_or("append registration unavailable")?;
        let state = entry.append.as_mut().ok_or("not an append registration")?;
        state.admission = state.admission.max(admission);
        Ok(())
    })
}

fn suffix(entry: &Entry, prompt: &str) -> bool {
    let Some(a) = &entry.append else { return false };
    let Some(start) = prompt.len().checked_sub(a.frame_bytes) else {
        return false;
    };
    let Some(tail) = prompt.get(start..) else {
        return false;
    };
    let prefix = &prompt[..start];
    hash(tail) == entry.hash
        && !prefix.contains(tail)
        && if a.separator {
            prefix.ends_with('\n')
        } else {
            prefix.is_empty()
        }
}

fn complete(record: &serde_json::Value) -> Option<(String, String, Option<String>)> {
    if record.get("type")?.as_str()? != "event_msg"
        || record.pointer("/payload/type")?.as_str()? != "item_completed"
        || record.pointer("/payload/item/type")?.as_str()? != "UserMessage"
    {
        return None;
    }
    let item = record.pointer("/payload/item")?;
    let row = item.get("id")?.as_str()?;
    if row.trim().is_empty() {
        return None;
    }
    let (last, images) = item.get("content")?.as_array()?.split_last()?;
    if !matches!(last.get("type")?.as_str()?, "text" | "input_text")
        || images.iter().any(|block| {
            block.get("type").and_then(serde_json::Value::as_str) != Some("local_image")
                || !block
                    .get("path")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|p| !p.is_empty())
        })
    {
        return None;
    }
    let text = last.get("text")?.as_str()?;
    let mut placeholders = 0;
    if let Some(elements) = last.get("text_elements") {
        for element in elements.as_array()? {
            let placeholder = element.get("placeholder")?.as_str()?;
            let image = placeholder.strip_prefix("[Image #")?.strip_suffix(']')?;
            if image.parse::<usize>().ok()? != placeholders + 1 {
                return None;
            }
            let start = usize::try_from(element.pointer("/byte_range/start")?.as_u64()?).ok()?;
            let end = usize::try_from(element.pointer("/byte_range/end")?.as_u64()?).ok()?;
            if text.get(start..end) != Some(placeholder) {
                return None;
            }
            placeholders += 1;
        }
    }
    if images.len() != placeholders {
        return None;
    }
    Some((
        text.to_string(),
        row.to_string(),
        record
            .pointer("/payload/turn_id")
            .and_then(serde_json::Value::as_str)
            .filter(|t| !t.trim().is_empty())
            .map(str::to_string),
    ))
}

pub(crate) fn bind_record(
    session: &str,
    transcript: &Path,
    start: u64,
    record: &serde_json::Value,
) -> Option<Receipt> {
    let (prompt, row, turn) = complete(record)?;
    let meta = std::fs::metadata(transcript).ok()?;
    if start >= meta.len() {
        return None;
    }
    let file = file_id(&meta);
    let stamp = incarnation(session, transcript);
    update(|store| {
        let candidate = |e: &Entry| {
            e.provider == "codex"
                && e.session == session
                && e.path == transcript
                && e.file == file
                && e.offset <= start
                && e.append.as_ref().is_some_and(|a| a.incarnation == stamp)
                && suffix(e, &prompt)
        };
        let key = format!("codex:{row}");
        let owner = store.entries.iter().position(|e| {
            e.path == transcript && e.file == file && e.events.iter().any(|event| event.key == key)
        });
        let index = if let Some(owner) = owner {
            let e = &store.entries[owner];
            if !candidate(e)
                || !e
                    .append
                    .as_ref()
                    .and_then(|a| a.receipt.as_ref())
                    .is_some_and(|r| {
                        r.row == row
                            && r.start == start
                            && r.prompt_hash == hash(&prompt)
                            && r.turn == turn
                    })
            {
                return Ok(None);
            }
            owner
        } else {
            let mut candidates = store
                .entries
                .iter()
                .enumerate()
                .filter(|(_, e)| candidate(e) && unbound(e));
            let Some((index, _)) = candidates.next() else {
                return Ok(None);
            };
            if candidates.next().is_some() {
                return Ok(None);
            }
            let e = &mut store.entries[index];
            e.append.as_mut().expect("append candidate").receipt = Some(Bound {
                row: row.clone(),
                start,
                prompt_hash: hash(&prompt),
                turn: turn.clone(),
            });
            e.events.push(Event {
                key,
                row: Some(row.clone()),
                kind: "codex".into(),
            });
            index
        };
        Ok(Some(projected(&store.entries[index])))
    })
    .ok()
    .flatten()
    .map(|attempt| Receipt {
        session: session.into(),
        attempt,
        prompt,
        row,
        native_turn: turn,
    })
}

pub(crate) fn bind_record_at(
    transcript: &Path,
    start: u64,
    record: &serde_json::Value,
) -> Option<Receipt> {
    if record
        .pointer("/payload/item/type")
        .and_then(serde_json::Value::as_str)
        != Some("UserMessage")
    {
        return None;
    }
    let store = read(&path().ok()?).ok()?;
    let mut sessions = store
        .entries
        .iter()
        .filter(|e| e.path == transcript && current(e))
        .map(|e| e.session.as_str());
    let session = sessions.next()?;
    if sessions.any(|other| other != session) {
        return None;
    }
    bind_record(session, transcript, start, record)
}

pub(crate) fn bind_open_record(
    file: &std::fs::File,
    start: u64,
    record: &serde_json::Value,
) -> Option<Receipt> {
    if record
        .pointer("/payload/item/type")
        .and_then(serde_json::Value::as_str)
        != Some("UserMessage")
    {
        return None;
    }
    let source = file_id(&file.metadata().ok()?);
    let (session, transcript) = {
        let store = read(&path().ok()?).ok()?;
        let mut owners = store
            .entries
            .iter()
            .filter(|e| e.file == source && current(e))
            .map(|e| (&e.session, &e.path));
        let owner = owners.next()?;
        if owners.any(|other| other != owner) {
            return None;
        }
        (owner.0.clone(), owner.1.clone())
    };
    bind_record(&session, &transcript, start, record)
}

pub(crate) fn projection(provider: &str, session: &str, id: &str) -> Option<Projection> {
    let store = read(&path().ok()?).ok()?;
    let entry = store
        .entries
        .iter()
        .find(|e| e.provider == provider && e.session == session && e.id == id && current(e))?;
    Some(projected(entry))
}

pub(crate) fn settle(id: &str, session: &str, row: &str, decision: Decision) -> bool {
    update(|store| {
        let Some(e) = store
            .entries
            .iter_mut()
            .find(|e| e.provider == "codex" && e.session == session && e.id == id && current(e))
        else {
            return Ok(false);
        };
        let a = e.append.as_mut().expect("current append");
        if !a.receipt.as_ref().is_some_and(|r| r.row == row) {
            return Ok(false);
        }
        if let Some(previous) = a.decision {
            return Ok(previous == decision);
        }
        a.decision = Some(decision);
        Ok(true)
    })
    .unwrap_or(false)
}

pub(super) fn observed(
    store: &Store,
    provider: &str,
    session: &str,
    prompts: &[String],
    row: Option<&str>,
) -> Option<ObservedAttempt> {
    if let Some(row) = row
        && store.entries.iter().any(|e| {
            e.append.is_none()
                && e.provider == provider
                && e.session == session
                && e.events
                    .iter()
                    .any(|event| event.row.as_deref() == Some(row))
                && prompts
                    .iter()
                    .any(|p| class_hashes(p).contains(&e.class_hash))
        })
    {
        return None;
    }
    let matching = |e: &&Entry| e.provider == provider && e.session == session && current(e);
    if let Some(row) = row
        && let Some(e) = store.entries.iter().filter(matching).find(|e| {
            e.append
                .as_ref()
                .and_then(|a| a.receipt.as_ref())
                .is_some_and(|r| r.row == row && prompts.iter().any(|p| hash(p) == r.prompt_hash))
        })
    {
        return Some(ObservedAttempt::Known(e.id.clone()));
    }
    store
        .entries
        .iter()
        .filter(matching)
        .any(|e| prompts.iter().any(|p| suffix(e, p)))
        .then_some(ObservedAttempt::Pending)
}

pub(crate) fn pending_candidate(
    provider: &str,
    session: &str,
    prompts: &[String],
    row: Option<&str>,
) -> bool {
    path().and_then(|p| read(&p)).is_ok_and(|store| {
        matches!(
            observed(&store, provider, session, prompts, row),
            Some(ObservedAttempt::Pending)
        )
    })
}

pub(crate) fn replay_start(session: &str, transcript: &Path) -> Option<u64> {
    let store = read(&path().ok()?).ok()?;
    store
        .entries
        .iter()
        .filter(|e| {
            e.provider == "codex"
                && e.session == session
                && e.path == transcript
                && current(e)
                && e.append.as_ref().is_some_and(|a| {
                    a.decision != Some(Decision::Joined)
                        && (a.admission != Admission::Reserved || a.receipt.is_some())
                })
        })
        .map(|e| e.offset)
        .min()
}

#[cfg(test)]
#[path = "append_tests.rs"]
mod tests;
