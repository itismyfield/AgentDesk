//! Durable identities for frames whose visible bytes are the ordinary user prompt.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub(crate) mod append;
#[cfg(test)]
mod checked_tests;
mod compat;
#[cfg(test)]
mod compat_tests;
#[cfg(unix)]
pub(crate) mod native_row;
pub(crate) mod notice_owner;
mod screen;
#[cfg(test)]
mod screen_tests;
pub(crate) mod v2;

const CAPACITY: usize = 4096;

#[derive(Clone, Serialize, Deserialize)]
struct Event {
    key: String,
    row: Option<String>,
    kind: String,
}

#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    id: String,
    provider: String,
    session: String,
    path: PathBuf,
    file: [u64; 2],
    offset: u64,
    author: String,
    hash: String,
    body_hash: String,
    class_hash: String,
    header: String,
    #[serde(default)]
    screen_prefix: String,
    events: Vec<Event>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    append: Option<append::State>,
}

#[derive(Default, Serialize, Deserialize)]
struct Store {
    entries: Vec<Entry>,
}

fn hash(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

fn class_inner(text: &str) -> Option<&str> {
    let text = text.trim();
    let token = |id: &str| {
        id.len() == 4
            && id
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    };
    text.strip_prefix("<pasted_content id=\"")
        .and_then(|rest| {
            let (id, rest) = rest.split_at_checked(4)?;
            token(id).then_some(())?;
            rest.strip_prefix("\">\n")?
                .strip_suffix(&format!("\n</pasted_content id=\"{id}\">"))
        })
        .or_else(|| {
            // A prefix reader can begin at the user header inside one complete paste block.
            text.starts_with("[User: ")
                .then(|| text.rsplit_once("\n</pasted_content id=\""))
                .flatten()
                .filter(|(_, end)| end.strip_suffix("\">").is_some_and(token))
                .map(|(inner, _)| inner)
        })
}

fn class_hashes(text: &str) -> Vec<String> {
    let text = text.trim();
    std::iter::once(hash(text))
        .chain(class_inner(text).map(|inner| hash(inner.trim())))
        .collect()
}

fn path() -> Result<PathBuf, String> {
    #[cfg(test)]
    {
        // Test subprocesses reopen their parent's temporary ledger to verify restart replay.
        if let Some(path) = std::env::var_os("ADK_TEST_FRAME_LEDGER_PATH") {
            let path = PathBuf::from(path);
            return path
                .starts_with(std::env::temp_dir())
                .then_some(path)
                .ok_or_else(|| "frame ledger test path outside temporary directory".into());
        }
        if let Some(root) = crate::config::runtime_root()
            && root.starts_with(std::env::temp_dir())
        {
            return Ok(root.join("runtime/injected_frames.json"));
        }
        thread_local! {
            static ROOT: tempfile::TempDir = tempfile::tempdir().expect("frame ledger test root");
        }
        return Ok(ROOT.with(|root| root.path().join("injected_frames.json")));
    }
    #[cfg(not(test))]
    crate::config::runtime_root()
        .map(|root| root.join("runtime/injected_frames.json"))
        .ok_or_else(|| "frame ledger root unavailable".into())
}

fn read(path: &Path) -> Result<Store, String> {
    compat::read_legacy(path)
}

/// What a non-scoped reader matches against: the V1 store, or a V2 history's owned entries.
fn readable(path: &Path) -> Result<Store, String> {
    if let Some(store) = v2::unscoped_store(path)? {
        return Ok(store);
    }
    read(path)
}

/// What the V1 append readers match against: the V1 store, or a V2 history's owned entries when
/// it holds any migrated append entry.
fn readable_append(path: &Path) -> Result<Store, String> {
    if let Some(store) = v2::append_store(path)? {
        return Ok(store);
    }
    read(path)
}

/// A V1 append mutation: on the V1 store, or on a V2 history's migrated append entries.
fn update_append<T>(apply: impl FnOnce(&mut Store) -> Result<T, String>) -> Result<T, String> {
    {
        let path = path()?;
        if v2::is_v2(&path)? {
            return v2::update_migrated(&path, apply);
        }
    }
    update(apply)
}

/// The attempt a diagnostic receipt thread records into, with no pane write right. A receipt is
/// evidence that the native record was observed, not that the provider ran the input.
pub(crate) struct ReceiptScope {
    context: Option<v2::ReceiptContext>,
}

impl ReceiptScope {
    pub(crate) fn capture() -> Self {
        Self {
            context: v2::ReceiptContext::capture(),
        }
    }

    pub(crate) fn enter<T>(&self, f: impl FnOnce() -> T) -> T {
        if let Some(context) = &self.context {
            return context.enter(f);
        }
        f()
    }
}

fn update<T>(apply: impl FnOnce(&mut Store) -> Result<T, String>) -> Result<T, String> {
    let path = path()?;
    std::fs::create_dir_all(path.parent().ok_or("frame ledger parent missing")?)
        .map_err(|error| error.to_string())?;
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path.with_extension("lock"))
        .map_err(|error| error.to_string())?;
    lock.lock().map_err(|error| error.to_string())?;
    let mut store = read(&path)?;
    let result = apply(&mut store)?;
    let raw = serde_json::to_string(&store).map_err(|error| error.to_string())?;
    crate::services::discord::runtime_store::atomic_write(&path, &raw)?;
    crate::services::discord::runtime_store::fsync_parent_dir(&path)
        .map_err(|error| error.to_string())?;
    Ok(result)
}

fn file_id(_meta: &std::fs::Metadata) -> [u64; 2] {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        [_meta.dev(), _meta.ino()]
    }
    #[cfg(not(unix))]
    [0, 0]
}

/// Attempts whose caller set up its own reader before the paste, kept for this process: who
/// answers a row is read from the attempt that took it, never from matching text.
static READERS: std::sync::Mutex<std::collections::VecDeque<[String; 3]>> =
    std::sync::Mutex::new(std::collections::VecDeque::new());
const READERS_KEPT: usize = 1024;

/// Records (`reads`) or drops whether attempt `id` has a reader of its own.
pub(crate) fn declare_reader(provider: &str, session: &str, id: &str, reads: bool) {
    let key = [provider.to_string(), session.to_string(), id.to_string()];
    let mut readers = READERS.lock().unwrap_or_else(|error| error.into_inner());
    readers.retain(|kept| *kept != key);
    if reads {
        if readers.len() >= READERS_KEPT {
            readers.pop_front();
        }
        readers.push_back(key);
    }
}

pub(crate) fn has_reader(provider: &str, session: &str, id: &str) -> bool {
    let readers = READERS.lock().unwrap_or_else(|error| error.into_inner());
    readers
        .iter()
        .any(|[p, s, i]| p == provider && s == session && i == id)
}

pub(crate) fn register(
    (provider, session, id, author): (&str, &str, &str, &str),
    transcript: &Path,
    offset: u64,
    frame: &str,
) -> Result<(), String> {
    if let Some(result) =
        v2::scoped_register((provider, session, id, author), transcript, offset, frame)
    {
        return result;
    }
    register_inner(
        (provider, session, id, author),
        transcript,
        offset,
        frame,
        None,
    )
}

fn register_inner(
    (provider, session, id, author): (&str, &str, &str, &str),
    transcript: &Path,
    offset: u64,
    frame: &str,
    append: Option<append::State>,
) -> Result<(), String> {
    let file = file_id(&std::fs::metadata(transcript).map_err(|error| error.to_string())?);
    let digest = hash(frame);
    update(|store| {
        if store
            .entries
            .iter()
            .any(|entry| entry.id == id && entry.path == transcript)
        {
            return Err("frame attempt already registered".into());
        }
        if store.entries.len() >= CAPACITY {
            return Err("frame ledger full".into());
        }
        if store.entries.iter().any(|entry| {
            entry.provider == provider
                && entry.session == session
                && entry.hash == digest
                && entry.path == transcript
                && entry.file == file
                && append::unbound(entry)
        }) {
            return Err("identical frame still unconfirmed".into());
        }
        store.entries.push(Entry {
            id: id.into(),
            provider: provider.into(),
            session: session.into(),
            path: transcript.into(),
            file,
            offset,
            author: author.into(),
            hash: digest,
            body_hash: hash(frame.split_once(']').map_or("", |(_, tail)| {
                tail.strip_prefix(' ')
                    .or_else(|| tail.strip_prefix('\n'))
                    .unwrap_or(tail)
            })),
            class_hash: hash(frame.trim()),
            header: frame
                .split_once(']')
                .map_or(frame, |(head, _)| head)
                .to_string()
                + "]",
            screen_prefix: screen::prefix(frame),
            events: Vec::new(),
            append,
        });
        Ok(())
    })
}

pub(crate) fn withdraw(id: &str, transcript: &Path) {
    // A scoped attempt keeps its registration and writes nothing; only the queue's exact
    // not-sent proof, projected as abandoned, releases its collision candidate.
    if v2::scoped() {
        return;
    }
    if let Err(error) = update(|store| {
        store.entries.retain(|entry| {
            entry.id != id
                || entry.path != transcript
                || !entry.events.is_empty()
                || entry
                    .append
                    .as_ref()
                    .is_some_and(|a| a.admission != append::Admission::Reserved)
        });
        Ok(())
    }) {
        tracing::warn!(%error, "unwritten frame ledger reservation retained");
    }
}

pub(crate) fn registered(id: &str, transcript: &Path, frame: &str) -> bool {
    if let Some(registered) = v2::scoped_registered(id, transcript, frame) {
        return registered;
    }
    let Ok(meta) = std::fs::metadata(transcript) else {
        return false;
    };
    let Some(store) = path().ok().and_then(|path| read(&path).ok()) else {
        return false;
    };
    store.entries.iter().any(|entry| {
        entry.id == id
            && entry.path == transcript
            && entry.file == file_id(&meta)
            && entry.hash == hash(frame)
    })
}

pub(crate) fn allows_legacy(id: &str, transcript: &Path) -> bool {
    if v2::scoped() {
        return false;
    }
    path().and_then(|path| read(&path)).is_ok_and(|store| {
        !store
            .entries
            .iter()
            .any(|entry| entry.id == id && entry.path == transcript)
    })
}

pub(crate) fn known(frame: &str) -> bool {
    // A rolled-back binary still recognizes frames a V2 history records; it never rewrites them.
    if let Some(known) = path()
        .ok()
        .and_then(|p| compat::known_v2(&p, &class_hashes(frame)))
    {
        return known;
    }
    let Some(store) = path().ok().and_then(|path| read(&path).ok()) else {
        return false;
    };
    let hashes = class_hashes(frame);
    store
        .entries
        .iter()
        .any(|entry| hashes.contains(&entry.class_hash))
}

pub(crate) fn screen_pending(session: &str, text: &str) -> bool {
    screen::pending(session, text)
}

/// Claims a complete exact record once; a known record identity cannot move between attempts.
pub(crate) fn receipt(
    id: &str,
    transcript: &Path,
    offset: u64,
    kind: &str,
    row: Option<&str>,
    frame: &str,
) -> Option<bool> {
    if let Some(receipt) = v2::scoped_receipt(id, transcript, offset, kind, row, frame) {
        return receipt;
    }
    let file = file_id(&std::fs::metadata(transcript).ok()?);
    update(|store| {
        let Some(target) = store.entries.iter().position(|entry| {
            entry.id == id
                && entry.path == transcript
                && entry.file == file
                && entry.hash == hash(frame)
                && entry.offset <= offset
                && entry.append.is_none()
        }) else {
            return Ok(None);
        };
        let key = row.map_or_else(|| format!("{kind}:{offset}"), |row| format!("{kind}:{row}"));
        let same = |entry: &Entry| entry.path == transcript && entry.file == file;
        if let Some(owner) = store
            .entries
            .iter()
            .position(|entry| same(entry) && entry.events.iter().any(|event| event.key == key))
        {
            return Ok((owner == target).then_some(true));
        }
        if matches!(kind, "command" | "user")
            && store.entries.iter().enumerate().any(|(index, entry)| {
                index != target
                    && same(entry)
                    && entry.hash == store.entries[target].hash
                    && entry.events.iter().any(|event| event.kind == "enqueue")
            })
        {
            return Ok(None);
        }
        let owner = target;
        store.entries[owner].events.push(Event {
            key,
            row: row.map(str::to_string),
            kind: kind.into(),
        });
        Ok((owner == target).then_some(true))
    })
    .ok()
    .flatten()
}

pub(crate) enum ObservedAttempt {
    Known(String),
    Pending,
    None,
}

pub(crate) fn observed_attempt(
    provider: &str,
    session: &str,
    prompts: &[String],
    row: Option<&str>,
) -> ObservedAttempt {
    observed_at(path(), provider, session, prompts, row)
}

/// Preserves unreadable evidence as an error; `logical_session` names the recorded input owner.
pub(crate) fn observed_attempt_checked(
    provider: &str,
    logical_session: &str,
    prompts: &[String],
    row: Option<&str>,
) -> Result<ObservedAttempt, String> {
    observed_at_checked(path(), provider, logical_session, prompts, row)
}

fn observed_at(
    path: Result<PathBuf, String>,
    provider: &str,
    session: &str,
    prompts: &[String],
    row: Option<&str>,
) -> ObservedAttempt {
    observed_at_checked(path, provider, session, prompts, row).unwrap_or_else(|_| {
        let plain = prompts
            .iter()
            .flat_map(|prompt| std::iter::once(prompt.trim()).chain(class_inner(prompt)))
            .any(|prompt| {
                prompt
                    .trim()
                    .strip_prefix("[User: ")
                    .and_then(|rest| rest.split_once(']'))
                    .and_then(|(head, _)| head.strip_suffix(')'))
                    .and_then(|head| head.rsplit_once(" (ID: "))
                    .is_some_and(|(name, author)| {
                        !name.is_empty()
                            && !author.is_empty()
                            && author.bytes().all(|byte| byte.is_ascii_digit())
                    })
            });
        if plain {
            ObservedAttempt::Pending
        } else {
            ObservedAttempt::None
        }
    })
}

fn observed_at_checked(
    path: Result<PathBuf, String>,
    provider: &str,
    session: &str,
    prompts: &[String],
    row: Option<&str>,
) -> Result<ObservedAttempt, String> {
    if let Some(observed) = v2::scoped_observed(provider, session, prompts, row) {
        return Ok(observed);
    }
    let store = path.and_then(|path| readable(&path))?;
    let hashes: Vec<_> = prompts
        .iter()
        .flat_map(|prompt| class_hashes(prompt))
        .collect();
    let matching = |entry: &Entry| {
        entry.append.is_none()
            && entry.provider == provider
            && entry.session == session
            && hashes.contains(&entry.class_hash)
    };
    if let Some(row) = row
        && let Some(entry) = store.entries.iter().find(|entry| {
            matching(entry)
                && entry
                    .events
                    .iter()
                    .any(|event| event.row.as_deref() == Some(row))
        })
    {
        return Ok(ObservedAttempt::Known(entry.id.clone()));
    }
    if let Some(observed) = append::observed(&store, provider, session, prompts, row) {
        return Ok(observed);
    }
    Ok(if store.entries.iter().any(matching) {
        ObservedAttempt::Pending
    } else {
        ObservedAttempt::None
    })
}

/// Who took a native row that carries a registered frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RowOwner {
    /// A non-meta user row: the attempt opened its own turn.
    User(String),
    /// A queued command the running turn took for the attempt.
    Command(String),
    /// No attempt registered before the row carries it.
    None,
    /// The transcript cannot be read now, so nothing is decided.
    Unread,
}

/// One row that took a frame: its byte offset, uuid, class hashes and whether it is a user row.
struct Taken {
    offset: u64,
    uuid: String,
    hashes: Vec<String>,
    user: bool,
}

/// The rows after `from` that take a human frame: non-meta user rows and queued prompt commands.
fn taken_rows(path: &Path, file: [u64; 2], from: u64) -> Option<Vec<Taken>> {
    use std::io::{BufRead, Seek, SeekFrom};
    let mut handle = std::fs::File::open(path).ok()?;
    (file_id(&handle.metadata().ok()?) == file).then_some(())?;
    handle.seek(SeekFrom::Start(from)).ok()?;
    let mut reader = std::io::BufReader::new(handle);
    let (mut rows, mut line, mut at) = (Vec::new(), Vec::new(), from);
    while reader.read_until(b'\n', &mut line).ok()? > 0 {
        let offset = at;
        at += line.len() as u64;
        let record: serde_json::Value = match line.ends_with(b"\n") {
            true => serde_json::from_slice(&line).unwrap_or_default(),
            false => serde_json::Value::Null,
        };
        line.clear();
        if let Some(taken) = taken_record(&record, offset) {
            rows.push(taken);
        }
    }
    Some(rows)
}

fn taken_text(record: &serde_json::Value) -> Option<String> {
    match record["type"].as_str() {
        Some("user")
            if record["isMeta"] != true
                && !crate::services::tui_o::shadow::identity::record_is_compact_summary(record) =>
        {
            match &record["message"]["content"] {
                serde_json::Value::String(text) => Some(text.clone()),
                serde_json::Value::Array(blocks) => blocks
                    .iter()
                    .map(|block| (block["type"] == "text").then(|| block["text"].as_str()))
                    .collect::<Option<Option<Vec<_>>>>()
                    .flatten()
                    .map(|texts| texts.join("\n")),
                _ => None,
            }
        }
        Some("attachment") if record["attachment"]["type"] == "queued_command" => {
            record["attachment"]["prompt"].as_str().map(str::to_string)
        }
        _ => None,
    }
}

fn taken_record(record: &serde_json::Value, offset: u64) -> Option<Taken> {
    let text = taken_text(record)?;
    let uuid = record["uuid"].as_str()?;
    Some(Taken {
        offset,
        uuid: uuid.into(),
        hashes: class_hashes(&text),
        user: record["type"] == "user",
    })
}

/// The attempt `row` took: a receipt's own row, else same-frame rows paired with attempts in
/// registration order, read afresh each time so rescans and later identical inputs agree.
pub(crate) fn row_owner(provider: &str, session: &str, prompts: &[String], row: &str) -> RowOwner {
    let Ok(store) = path().and_then(|path| readable(&path)) else {
        return RowOwner::Unread;
    };
    let hashes: Vec<_> = prompts.iter().flat_map(|p| class_hashes(p)).collect();
    owner_in(&store, (provider, session), &hashes, row)
}

/// A queue-owned Discord registration remains proof after its display fields are compacted.
pub(crate) fn discord_owner_attempt(
    session: &str,
    native_session: &str,
    transcript: &Path,
    id: &str,
) -> Result<bool, String> {
    Ok(discord_stop_entries(session, native_session, transcript)?
        .iter()
        .any(|entry| entry.id == id))
}

fn discord_stop_entries(
    session: &str,
    native_session: &str,
    transcript: &Path,
) -> Result<Vec<Entry>, String> {
    let raw = std::fs::read(path()?).map_err(|error| error.to_string())?;
    let value = compat::parse(&raw)?;
    if !compat::envelope(&value)? {
        return serde_json::from_value::<Store>(value)
            .map(|_| Vec::new())
            .map_err(|error| error.to_string());
    }
    let snapshot: compat::HistoryV2 =
        serde_json::from_value(value.clone()).map_err(|error| error.to_string())?;
    let file = file_id(&std::fs::metadata(transcript).map_err(|error| error.to_string())?);
    let mut entries: Vec<_> = snapshot
        .live
        .into_iter()
        .zip(value["live"].as_array().into_iter().flatten())
        .map(|(entry, bound)| (entry, &bound["registration"]))
        .collect();
    for passive in &snapshot.passive {
        if passive["state"] == "abandoned" || passive["entry"].is_null() {
            continue;
        }
        let entry: Entry =
            serde_json::from_value(passive["entry"].clone()).map_err(|error| error.to_string())?;
        entries.push((entry, &passive["registration"]));
    }
    let mut accepted = Vec::new();
    for (entry, registration) in entries {
        let key = &registration["key"];
        let identity = &key["identity"];
        let discord_key = |value: &serde_json::Value| {
            value
                .as_u64()
                .is_some_and(crate::services::tui_input::input_key::is_discord_key)
        };
        if entry.provider != "claude"
            || entry.session != session
            || entry.path != transcript
            || entry.file != file
            || entry.append.is_some()
            || key["attempt"]["nonce"].as_str() != Some(entry.id.as_str())
            || registration["frame"]["class_hash"].as_str() != Some(entry.class_hash.as_str())
            || !discord_key(&identity["primary"])
            || !identity["sources"].as_array().is_some_and(|sources| {
                !sources.is_empty()
                    && sources
                        .iter()
                        .all(|source| discord_key(&source["message_id"]))
            })
        {
            continue;
        }
        let source: v2::Source = serde_json::from_value(registration["source"].clone())
            .map_err(|error| error.to_string())?;
        source.validate(key)?;
        if source.provider == "claude"
            && source.target["owner"] == session
            && source.session_id == native_session
            && source.transcript == transcript
            && source.file == file
            && source.offset == entry.offset
        {
            accepted.push(entry);
        }
    }
    accepted.sort_by_key(|entry| entry.offset);
    Ok(accepted)
}

/// Enqueue positions pair once with exact, source-bound Discord registrations, never body text alone.
pub(crate) fn discord_enqueue_owners(
    session: &str,
    native_session: &str,
    transcript: &Path,
    opener_end: u64,
    enqueues: &[(u64, String)],
    reserved: &[String],
) -> Result<Vec<RowOwner>, String> {
    let entries: Vec<_> = discord_stop_entries(session, native_session, transcript)?
        .into_iter()
        .filter(|entry| entry.offset >= opener_end && !reserved.contains(&entry.id))
        .collect();
    let mut by_frame = std::collections::BTreeMap::<String, Vec<usize>>::new();
    for (index, (_, body)) in enqueues.iter().enumerate() {
        by_frame.entry(hash(body.trim())).or_default().push(index);
    }
    let mut registered = std::collections::BTreeMap::<&str, Vec<&Entry>>::new();
    for entry in &entries {
        registered
            .entry(entry.class_hash.as_str())
            .or_default()
            .push(entry);
    }
    let mut owners = vec![RowOwner::None; enqueues.len()];
    for (frame_hash, indexes) in by_frame {
        let last = indexes
            .iter()
            .map(|index| enqueues[*index].0)
            .max()
            .unwrap_or(0);
        let Some(registered) = registered.get(frame_hash.as_str()) else {
            continue;
        };
        let candidates: Vec<_> = registered
            .iter()
            .copied()
            .filter(|entry| entry.offset <= last)
            .collect();
        let Some(first) = candidates.first() else {
            continue;
        };
        let queued: Vec<_> = indexes
            .into_iter()
            .filter(|index| enqueues[*index].0 >= first.offset)
            .collect();
        // Extra identical inputs or reservations make an absorption's owner unknowable.
        if candidates.len() != queued.len()
            || candidates
                .windows(2)
                .any(|pair| pair[0].offset == pair[1].offset)
            || candidates
                .iter()
                .zip(&queued)
                .any(|(entry, index)| entry.offset > enqueues[*index].0)
        {
            for index in queued {
                owners[index] = RowOwner::Unread;
            }
            continue;
        }
        for (entry, index) in candidates.into_iter().zip(queued) {
            owners[index] = RowOwner::Command(entry.id.clone());
        }
    }
    Ok(owners)
}

fn owner_in(
    store: &Store,
    (provider, session): (&str, &str),
    hashes: &[String],
    row: &str,
) -> RowOwner {
    let same = |entry: &&Entry| {
        entry.append.is_none()
            && entry.provider == provider
            && entry.session == session
            && hashes.contains(&entry.class_hash)
    };
    let mut files: Vec<(&Path, [u64; 2])> = Vec::new();
    for entry in store.entries.iter().filter(same) {
        if !files.contains(&(entry.path.as_path(), entry.file)) {
            files.push((entry.path.as_path(), entry.file));
        }
    }
    for (path, file) in files {
        let mut entries: Vec<&Entry> = (store.entries.iter().filter(same))
            .filter(|entry| entry.path == path && entry.file == file)
            .collect();
        entries.sort_by_key(|entry| entry.offset);
        let from = entries.first().map_or(0, |entry| entry.offset);
        let Some(rows) = taken_rows(path, file, from) else {
            return RowOwner::Unread;
        };
        let Some(this) = rows.iter().find(|taken| taken.uuid == row) else {
            continue;
        };
        return match pair(&entries, &rows, row) {
            Some(entry) if this.user => RowOwner::User(entry.id.clone()),
            Some(entry) => RowOwner::Command(entry.id.clone()),
            None => RowOwner::None,
        };
    }
    RowOwner::None
}

/// The entry that took `row`: the one whose receipt names it, else the earliest free entry each
/// earlier unreceipted row did not take.
fn pair<'a>(entries: &[&'a Entry], rows: &[Taken], row: &str) -> Option<&'a Entry> {
    let receipt = |entry: &Entry, uuid: &str| {
        (entry.events.iter()).any(|event| {
            matches!(event.kind.as_str(), "user" | "command") && event.row.as_deref() == Some(uuid)
        })
    };
    if let Some(entry) = entries.iter().find(|entry| receipt(entry, row)) {
        return Some(entry);
    }
    // An entry a receipt paired with another row takes no other row.
    let mut free: Vec<&Entry> = (entries.iter().copied())
        .filter(|entry| !rows.iter().any(|taken| receipt(entry, &taken.uuid)))
        .collect();
    for taken in rows
        .iter()
        .filter(|taken| !entries.iter().any(|e| receipt(e, &taken.uuid)))
    {
        let at = free.iter().position(|entry| {
            entry.offset <= taken.offset && taken.hashes.contains(&entry.class_hash)
        });
        let Some(at) = at else {
            continue;
        };
        let entry = free.remove(at);
        if taken.uuid == row {
            return Some(entry);
        }
    }
    None
}

/// The row attempt `id` took in its transcript as [`row_owner`] pairs them, and whether that row
/// is a user row.
pub(crate) fn attempt_row(provider: &str, session: &str, id: &str) -> Option<(u64, bool)> {
    let store = path().and_then(|path| readable(&path)).ok()?;
    let entry = store.entries.iter().find(|entry| entry.id == id)?;
    let hashes = [entry.class_hash.clone()];
    let rows = taken_rows(&entry.path, entry.file, entry.offset)?;
    rows.into_iter()
        .filter(|taken| taken.hashes.contains(&entry.class_hash))
        .find_map(
            |taken| match owner_in(&store, (provider, session), &hashes, &taken.uuid) {
                RowOwner::User(owner) | RowOwner::Command(owner) if owner == id => {
                    Some((taken.offset, taken.user))
                }
                _ => None,
            },
        )
}

pub(crate) fn receipt_row(id: &str, transcript: &Path) -> Option<String> {
    if let Some(row) = v2::scoped_receipt_row(id, transcript) {
        return row;
    }
    let store = read(&path().ok()?).ok()?;
    store
        .entries
        .iter()
        .find(|entry| entry.id == id && entry.path == transcript)?
        .events
        .iter()
        .find_map(|event| event.row.clone())
}

pub(crate) fn unique_attempt(id: &str, transcript: &Path, frame: &str) -> bool {
    let Some(store) = path().ok().and_then(|path| read(&path).ok()) else {
        return false;
    };
    let matching: Vec<_> = store
        .entries
        .iter()
        .filter(|entry| entry.path == transcript && entry.hash == hash(frame))
        .collect();
    matches!(matching.as_slice(), [entry] if entry.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(label: &str) -> (tempfile::TempDir, PathBuf, String, String) {
        let root = tempfile::tempdir().unwrap();
        let transcript = root.path().join("native.jsonl");
        std::fs::write(&transcript, "").unwrap();
        let session = format!("{label}-{}", uuid::Uuid::new_v4());
        let frame = super::super::idle("ann", "720101", &session);
        (root, transcript, session, frame)
    }

    fn assert_replayed(session: &str, frame: &str) {
        use crate::services::tui_prompt_dedupe::{
            PromptObservation, observe_prompt_by_tmux_with_entry_id_at,
        };
        let wrapped =
            format!("<pasted_content id=\"dfd9\">\n{frame}\n</pasted_content id=\"dfd9\">");
        let forms = [
            frame.to_string(),
            wrapped.clone(),
            format!("\n\n{wrapped}\n"),
            format!("{frame}\n</pasted_content id=\"dfd9\">"),
        ];
        for (index, form) in forms.iter().enumerate() {
            assert!(known(form), "replay form {index}");
            assert!(matches!(
                observed_attempt("claude", session, std::slice::from_ref(form), None),
                ObservedAttempt::Pending
            ));
            assert_eq!(
                observe_prompt_by_tmux_with_entry_id_at(
                    "claude",
                    session,
                    form,
                    Some(&format!("replay-row-{index}")),
                    chrono::Utc::now()
                ),
                PromptObservation::InjectedSteer,
                "actual observer replay form {index}"
            );
        }
        assert!(super::super::opens_inject(frame));
        assert!(!super::super::opens_inject(session));
        assert!(matches!(
            observed_attempt("claude", session, &[session.to_string()], None),
            ObservedAttempt::None
        ));
        for foreign in [
            wrapped.replace(
                "</pasted_content id=\"dfd9\">",
                "</pasted_content id=\"aaaa\">",
            ),
            format!("{frame}\nforeign input"),
            wrapped.replace(frame, &format!("foreign input\n{frame}")),
            format!("{wrapped}\n{wrapped}"),
            frame.replace("(ID: 720101)", "(ID: 720102)"),
        ] {
            assert!(!known(&foreign), "foreign input is not our exact frame");
            assert!(matches!(
                observed_attempt("claude", session, std::slice::from_ref(&foreign), None),
                ObservedAttempt::None
            ));
            assert!(!matches!(
                observe_prompt_by_tmux_with_entry_id_at(
                    "claude",
                    session,
                    &foreign,
                    None,
                    chrono::Utc::now()
                ),
                PromptObservation::InjectedSteer | PromptObservation::InjectedDeferred
            ));
        }
    }

    #[test]
    fn durable_frame_replay_is_injected_and_headerless_human_is_not() {
        let _runtime = crate::config::TestRuntimeRootGuard::new();
        if let Ok(probe) = std::env::var("ADK_TEST_FRAME_REPLAY_ONLY") {
            let (session, frame): (String, String) = serde_json::from_str(&probe).unwrap();
            assert_replayed(&session, &frame);
            return;
        }
        let (_root, transcript, session, frame) = setup("claude-replay");
        register(
            ("claude", &session, "ca720101", "720101"),
            &transcript,
            0,
            &frame,
        )
        .unwrap();
        assert_replayed(&session, &frame);
        let persisted = read(&path().unwrap()).unwrap();
        let entry = persisted
            .entries
            .iter()
            .find(|entry| entry.id == "ca720101" && entry.path == transcript)
            .unwrap();
        assert_eq!(entry.author, "720101");
        assert_eq!(entry.body_hash, hash(&session));
        assert_ne!(entry.hash, entry.body_hash);
        assert!(
            known(&frame),
            "classification reloads durable state without a process registry"
        );
        let restarted = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "services::tui_input::human_frame::history::tests::durable_frame_replay_is_injected_and_headerless_human_is_not",
                "--nocapture",
            ])
            .env("ADK_TEST_FRAME_LEDGER_PATH", path().unwrap())
            .env(
                "ADK_TEST_FRAME_REPLAY_ONLY",
                serde_json::to_string(&(&session, &frame)).unwrap(),
            )
            .output()
            .unwrap();
        assert!(
            restarted.status.success(),
            "fresh-process observer failed: {}{}",
            String::from_utf8_lossy(&restarted.stdout),
            String::from_utf8_lossy(&restarted.stderr)
        );
        assert!(
            String::from_utf8_lossy(&restarted.stdout).contains("1 passed"),
            "fresh-process replay test was not selected"
        );
    }

    #[test]
    fn codex_receipt_identity_selects_each_identical_attempt_and_replays_its_own_entry() {
        use crate::services::tui_prompt_dedupe::{
            PromptObservation, observe_codex_prompt_in_turn_at, register_injected_steer,
        };
        let _runtime = crate::config::TestRuntimeRootGuard::new();
        let (_root, transcript, session, frame) = setup("codex-order");
        register(
            ("codex", &session, "ca720102", "720101"),
            &transcript,
            0,
            &frame,
        )
        .unwrap();
        assert!(register_injected_steer(
            "codex",
            &session,
            "ca720102",
            "target",
            Some(1)
        ));
        assert_eq!(
            receipt("ca720102", &transcript, 0, "codex", Some("row-a"), &frame),
            Some(true)
        );
        register(
            ("codex", &session, "ca720103", "720101"),
            &transcript,
            1,
            &frame,
        )
        .unwrap();
        assert!(register_injected_steer(
            "codex",
            &session,
            "ca720103",
            "target",
            Some(2)
        ));
        assert_eq!(
            receipt("ca720103", &transcript, 2, "codex", Some("row-a"), &frame),
            None
        );
        assert!(matches!(
            observed_attempt(
                "codex",
                &session,
                std::slice::from_ref(&frame),
                Some("unknown-row")
            ),
            ObservedAttempt::Pending
        ));
        assert_eq!(
            observe_codex_prompt_in_turn_at(
                &session,
                &frame,
                Some("row-a"),
                Some("target"),
                chrono::Utc::now()
            ),
            PromptObservation::InjectedSteer
        );
        assert_eq!(
            receipt("ca720103", &transcript, 3, "codex", Some("row-b"), &frame),
            Some(true)
        );
        for row in ["row-a", "row-b", "row-a", "row-b"] {
            assert_eq!(
                observe_codex_prompt_in_turn_at(
                    &session,
                    &frame,
                    Some(row),
                    Some("target"),
                    chrono::Utc::now()
                ),
                PromptObservation::InjectedSteer
            );
        }
        assert!(
            !unique_attempt("ca720103", &transcript, &frame),
            "ambiguous identical frames cannot mend by body alone"
        );
        let (_cold_root, cold_transcript, cold_session, cold_frame) = setup("codex-cold");
        register(
            ("codex", &cold_session, "ca720108", "720101"),
            &cold_transcript,
            0,
            &cold_frame,
        )
        .unwrap();
        for has_receipt in [false, true] {
            if has_receipt {
                assert_eq!(
                    receipt(
                        "ca720108",
                        &cold_transcript,
                        0,
                        "codex",
                        Some("cold-row"),
                        &cold_frame
                    ),
                    Some(true)
                );
            }
            assert_eq!(
                observe_codex_prompt_in_turn_at(
                    &cold_session,
                    &cold_frame,
                    Some("cold-row"),
                    Some("target"),
                    chrono::Utc::now()
                ),
                PromptObservation::InjectedDeferred
            );
        }
        assert!(register_injected_steer(
            "codex",
            &cold_session,
            "ca720109",
            "another-target",
            Some(3)
        ));
        assert_eq!(
            observe_codex_prompt_in_turn_at(
                &cold_session,
                &cold_frame,
                Some("cold-row"),
                Some("target"),
                chrono::Utc::now()
            ),
            PromptObservation::InjectedDeferred,
            "a durable identity absent from a nonempty memory ledger stays deferred"
        );
    }

    #[test]
    fn unreadable_ledger_defers_plain_user_frames_and_leaves_headerless_humans_alone() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("bad-ledger.json");
        std::fs::write(&path, "{").unwrap();
        let frame = super::super::idle("ann", "720101", "known input");
        let wrapped =
            format!("<pasted_content id=\"dfd9\">\n{frame}\n</pasted_content id=\"dfd9\">");
        let forms = [
            frame.clone(),
            wrapped.clone(),
            format!("\n\n{wrapped}\n"),
            format!("{frame}\n</pasted_content id=\"dfd9\">"),
        ];
        for path in [Ok(path), Err("root unavailable".into())] {
            for form in &forms {
                assert!(matches!(
                    observed_at(
                        path.clone(),
                        "claude",
                        "session",
                        std::slice::from_ref(form),
                        None
                    ),
                    ObservedAttempt::Pending
                ));
            }
            assert!(matches!(
                observed_at(path, "claude", "session", &["human input".into()], None),
                ObservedAttempt::None
            ));
        }
    }
}
