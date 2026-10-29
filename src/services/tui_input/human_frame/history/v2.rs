//! Queue-owned frame registrations; passive evidence never consumes a live slot.
use super::*;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

mod collision;
mod migration;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Source {
    pub(crate) target: Value,
    pub(crate) provider: String,
    pub(crate) session_id: String,
    pub(crate) transcript: PathBuf,
    pub(crate) file: [u64; 2],
    pub(crate) offset: u64,
    pub(crate) binding_watermark: Option<String>,
}

impl Source {
    pub(crate) fn validate(&self, key: &Value) -> Result<(), String> {
        let attempt = &key["attempt"];
        let expected = &attempt["source"];
        let meta = std::fs::metadata(&self.transcript).map_err(|e| e.to_string())?;
        if attempt["provider"] != self.provider
            || attempt["target"] != self.target
            || self.session_id.is_empty()
            || self.file == [0, 0]
            || file_id(&meta) != self.file
            || meta.len() < self.offset
            || self.target["execution_nonce"]
                .as_str()
                .is_none_or(str::is_empty)
        {
            return Err("source execution/file identity mismatch".into());
        }
        let matches = match expected["kind"].as_str() {
            Some("existing") => {
                expected["session_id"] == self.session_id
                    && expected["transcript"] == self.transcript.display().to_string()
                    && expected["file"] == json!(self.file)
                    && expected["offset"] == self.offset
            }
            Some("pre_source") => [
                (&expected["expected_session_id"], json!(self.session_id)),
                (&expected["expected_transcript"], json!(self.transcript)),
                (
                    &expected["binding_watermark"],
                    json!(self.binding_watermark),
                ),
            ]
            .iter()
            .all(|(wanted, actual)| wanted.is_null() || *wanted == actual),
            _ => false,
        };
        matches
            .then_some(())
            .ok_or_else(|| "source expectation mismatch".into())
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Frame {
    author: String,
    hash: String,
    body_hash: String,
    class_hash: String,
    header: String,
    screen_prefix: String,
}
impl Frame {
    fn new(author: &str, text: &str) -> Self {
        Self {
            author: author.into(),
            hash: hash(text),
            class_hash: hash(text.trim()),
            body_hash: hash(text.split_once(']').map_or("", |(_, tail)| {
                tail.strip_prefix(' ')
                    .or_else(|| tail.strip_prefix('\n'))
                    .unwrap_or(tail)
            })),
            header: text
                .split_once(']')
                .map_or(text, |(head, _)| head)
                .to_string()
                + "]",
            screen_prefix: screen::prefix(text),
        }
    }
    fn bind(&self, key: &Value, source: &Source) -> Entry {
        Entry {
            id: key["attempt"]["nonce"].as_str().unwrap_or_default().into(),
            provider: source.provider.clone(),
            session: source.target["owner"].as_str().unwrap_or_default().into(),
            path: source.transcript.clone(),
            file: source.file,
            offset: source.offset,
            author: self.author.clone(),
            hash: self.hash.clone(),
            body_hash: self.body_hash.clone(),
            class_hash: self.class_hash.clone(),
            header: self.header.clone(),
            screen_prefix: self.screen_prefix.clone(),
            events: vec![],
            append: None,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Registration {
    namespace: String,
    key: Value,
    frame: Frame,
    #[serde(default)]
    source: Option<Source>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Bound {
    #[serde(flatten)]
    entry: Entry,
    registration: Registration,
}
#[derive(Clone, Serialize, Deserialize)]
struct Passive {
    registration: Option<Registration>,
    entry: Option<Entry>,
    state: String,
    #[serde(skip)]
    encoded: Option<String>,
}
#[derive(Default, Serialize, Deserialize)]
struct Snapshot {
    version: u32,
    live: Vec<Bound>,
    #[serde(default)]
    planned: Vec<Registration>,
    passive: Vec<Passive>,
}
impl Snapshot {
    fn encode(&mut self) -> Result<String, String> {
        let mut raw = format!(
            "{{\"version\":{},\"live\":{},\"planned\":{},\"passive\":[",
            self.version,
            serde_json::to_string(&self.live).map_err(|e| e.to_string())?,
            serde_json::to_string(&self.planned).map_err(|e| e.to_string())?
        );
        for (index, passive) in self.passive.iter_mut().enumerate() {
            if passive.encoded.is_none() {
                passive.encoded = Some(serde_json::to_string(&passive).map_err(|e| e.to_string())?);
            }
            if index > 0 {
                raw.push(',');
            }
            raw.push_str(
                passive
                    .encoded
                    .as_deref()
                    .ok_or("missing passive encoding")?,
            );
        }
        raw.push_str("]}");
        Ok(raw)
    }
    fn locate(&self, namespace: &str, key: &Value) -> Option<(&Registration, Option<&Entry>)> {
        self.live
            .iter()
            .map(|b| (&b.registration, Some(&b.entry)))
            .chain(self.planned.iter().map(|r| (r, None)))
            .chain(
                self.passive
                    .iter()
                    .filter_map(|p| p.registration.as_ref().map(|r| (r, p.entry.as_ref()))),
            )
            .find(|(r, _)| r.namespace == namespace && r.key == *key)
    }
    /// The queue proved this attempt was never sent, so no native row can be its own.
    fn abandoned(&self, namespace: &str, key: &Value) -> bool {
        self.passive.iter().any(|p| {
            p.state == collision::ABANDONED
                && p.registration
                    .as_ref()
                    .is_some_and(|r| r.namespace == namespace && r.key == *key)
        })
    }
    fn entries(&self) -> impl Iterator<Item = (Option<&Registration>, &Entry)> {
        self.live
            .iter()
            .map(|b| (Some(&b.registration), &b.entry))
            .chain(
                self.passive
                    .iter()
                    .filter_map(|p| p.entry.as_ref().map(|e| (p.registration.as_ref(), e))),
            )
    }
}

#[derive(Clone)]
pub(crate) struct Registry {
    path: PathBuf,
    cached: Arc<Mutex<Option<(String, Snapshot)>>>,
    /// (old, candidate) of a publication whose durability is unconfirmed.
    pending: Arc<Mutex<Option<(String, String)>>>,
    encode_fault: Arc<std::sync::atomic::AtomicBool>,
}
impl Registry {
    pub(crate) fn at(path: PathBuf) -> Self {
        Self {
            path,
            cached: Arc::new(Mutex::new(None)),
            pending: Arc::new(Mutex::new(None)),
            encode_fault: Arc::default(),
        }
    }
    pub(crate) fn runtime() -> Result<Self, String> {
        Ok(Self::at(path()?))
    }
    fn transaction<T>(
        &self,
        apply: impl FnOnce(&mut Snapshot) -> Result<T, String>,
    ) -> Result<T, String> {
        use crate::services::discord::runtime_store::{
            AtomicWriteContext, durable_publish::FlushStatus, durable_publish::*,
        };
        let mut cached = self.cached.lock().unwrap_or_else(|e| e.into_inner());
        std::fs::create_dir_all(self.path.parent().ok_or("history parent missing")?)
            .map_err(|e| e.to_string())?;
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.path.with_extension("lock"))
            .map_err(|e| e.to_string())?;
        lock.lock().map_err(|e| e.to_string())?;
        let raw = match std::fs::read_to_string(&self.path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => migration::MISSING.into(),
            Err(e) => return Err(e.to_string()),
        };
        let context = || AtomicWriteContext::new("injected_frames_v2");
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((old, candidate)) = pending.as_ref() {
            // The unconfirmed candidate must become durable, or the old bytes must still stand.
            if raw == *candidate {
                if retry_published_status(&self.path, context()) != FlushStatus::Flushed {
                    return Err("history candidate durability unconfirmed".into());
                }
            } else if raw != *old {
                return Err(
                    "history changed after an unconfirmed publication; preserve both".into(),
                );
            }
            *pending = None;
        }
        if cached.as_ref().is_none_or(|(bytes, _)| *bytes != raw) {
            *cached = Some((raw.clone(), migration::restore(&raw)?));
        }
        let (_, state) = cached.as_mut().ok_or("history cache unavailable")?;
        let encoded = apply(state).and_then(|result| {
            if self.encode_fault.load(std::sync::atomic::Ordering::SeqCst) {
                return Err("history encoding failed".to_string());
            }
            Ok((result, state.encode()?))
        });
        let (result, candidate) = match encoded {
            Ok(value) => value,
            Err(error) => {
                *cached = None;
                return Err(error);
            }
        };
        if candidate == raw {
            return Ok(result);
        }
        match critical_publish_status(&self.path, &candidate, context()) {
            Ok(FlushStatus::Flushed) => {
                cached.as_mut().ok_or("history cache disappeared")?.0 = candidate;
                Ok(result)
            }
            status => {
                *cached = None;
                if status.is_ok() {
                    *pending = Some((raw, candidate));
                }
                Err("history publication unconfirmed; preserve registration".into())
            }
        }
    }
    pub(crate) fn prepare(&self, namespace: &str, input: &Value) -> Result<(), String> {
        let key = json!({"identity":input["identity"],"attempt":input["attempt"]});
        let frame = input["payload"]["frame"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or("unprepared frame")?;
        let author = input["original"]["author_id"]
            .as_u64()
            .ok_or("missing author")?
            .to_string();
        let source = if let Some(source) = input.get("resolved_source").filter(|s| !s.is_null()) {
            Some(serde_json::from_value::<Source>(source.clone()).map_err(|e| e.to_string())?)
        } else if key["attempt"]["source"]["kind"] == "existing" {
            let s = &key["attempt"]["source"];
            Some(serde_json::from_value::<Source>(json!({"target":key["attempt"]["target"],
                "provider":key["attempt"]["provider"],"session_id":s["session_id"],
                "transcript":s["transcript"],"file":s["file"],"offset":s["offset"],"binding_watermark":null})).map_err(|e| e.to_string())?)
        } else {
            None
        };
        if let Some(source) = &source {
            source.validate(&key)?;
        }
        if namespace.is_empty()
            || key["attempt"]["nonce"].as_str().is_none_or(str::is_empty)
            || key["identity"]["sources"]
                .as_array()
                .is_none_or(Vec::is_empty)
        {
            return Err("missing full attempt identity".into());
        }
        let bind = self.transaction(|state| {
            if let Some((registered, _)) = state.locate(namespace, &key) {
                if registered.frame.hash != hash(frame)
                    || registered
                        .source
                        .as_ref()
                        .zip(source.as_ref())
                        .is_some_and(|(a, b)| a != b)
                {
                    return Err("immutable history registration changed".into());
                }
                return Ok(registered.source.is_none() && source.is_some());
            }
            if state.live.len() + state.planned.len() >= CAPACITY {
                return Err("active frame slots full".into());
            }
            let registration = Registration {
                namespace: namespace.into(),
                key: key.clone(),
                frame: Frame::new(&author, frame),
                source: source.clone(),
            };
            if let Some(source) = &registration.source {
                state.live.push(Bound {
                    entry: registration.frame.bind(&registration.key, source),
                    registration,
                });
            } else {
                state.planned.push(registration);
            }
            Ok(false)
        })?;
        if bind {
            self.bind(
                namespace,
                &key,
                source.as_ref().ok_or("missing resolved source")?,
            )?;
        }
        Ok(())
    }
    pub(crate) fn bind(&self, namespace: &str, key: &Value, source: &Source) -> Result<(), String> {
        source.validate(key)?;
        self.transaction(|state| {
            if let Some((registered, _)) = state.locate(namespace, key)
                && let Some(old) = &registered.source
            {
                return (old == source)
                    .then_some(())
                    .ok_or_else(|| "source already bound".into());
            }
            if let Some(at) = state
                .planned
                .iter()
                .position(|r| r.namespace == namespace && r.key == *key)
            {
                let mut registration = state.planned.remove(at);
                registration.source = Some(source.clone());
                state.live.push(Bound {
                    entry: registration.frame.bind(key, source),
                    registration,
                });
                return Ok(());
            }
            let passive = state
                .passive
                .iter_mut()
                .find(|p| {
                    p.registration
                        .as_ref()
                        .is_some_and(|r| r.namespace == namespace && r.key == *key)
                })
                .ok_or("attempt registration missing")?;
            let registration = passive.registration.as_mut().ok_or("legacy registration")?;
            registration.source = Some(source.clone());
            passive.entry = Some(registration.frame.bind(key, source));
            passive.encoded = None;
            Ok(())
        })
    }
    pub(crate) fn project(&self, snapshot: &Value) -> Result<(), String> {
        let namespace = snapshot["namespace"]
            .as_str()
            .ok_or("missing queue namespace")?;
        self.transaction(|state| {
            let disposition = |r: &Registration| -> Option<&'static str> {
                if r.namespace != namespace {
                    return None;
                }
                let same = |input: &Value| {
                    input["identity"] == r.key["identity"] && input["attempt"] == r.key["attempt"]
                };
                if same(&snapshot["head_injection"]) {
                    return None;
                }
                if let Some(retained) = snapshot["retained_inputs"]
                    .as_array()
                    .and_then(|xs| xs.iter().find(|x| same(&x["input"])))
                {
                    // Only the queue's exact not-sent proof on a Reserved input frees the candidate.
                    let abandoned = retained["disposition"] == "known_not_sent"
                        && retained["input"]["phase"] == "reserved";
                    return Some(if abandoned {
                        collision::ABANDONED
                    } else {
                        "retained"
                    });
                }
                if snapshot["terminal_identities"]
                    .as_array()
                    .is_some_and(|xs| xs.iter().any(|x| x["identity"] == r.key["identity"]))
                {
                    return Some("settled");
                }
                Some("retained")
            };
            let mut live = Vec::new();
            for bound in state.live.drain(..) {
                if let Some(status) = disposition(&bound.registration) {
                    state.passive.push(Passive {
                        registration: Some(bound.registration),
                        entry: Some(bound.entry),
                        state: status.into(),
                        encoded: None,
                    });
                } else {
                    live.push(bound);
                }
            }
            state.live = live;
            let mut planned = Vec::new();
            for registration in state.planned.drain(..) {
                if let Some(status) = disposition(&registration) {
                    state.passive.push(Passive {
                        registration: Some(registration),
                        entry: None,
                        state: status.into(),
                        encoded: None,
                    });
                } else {
                    planned.push(registration);
                }
            }
            state.planned = planned;
            // Evidence only advances: retained to abandoned or settled, abandoned to settled.
            let rank = |state: &str| {
                ["retained", collision::ABANDONED, "settled"]
                    .iter()
                    .position(|s| *s == state)
            };
            for passive in &mut state.passive {
                if let Some(r) = &passive.registration
                    && let Some(next) = disposition(r)
                    && rank(next) > rank(&passive.state)
                {
                    passive.state = next.into();
                    passive.encoded = None;
                }
            }
            Ok(())
        })
    }
    fn inspect<T>(&self, apply: impl FnOnce(&Snapshot) -> Result<T, String>) -> Result<T, String> {
        let mut cache = self.cached.lock().unwrap_or_else(|e| e.into_inner());
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.path.with_extension("lock"))
            .map_err(|e| e.to_string())?;
        lock.lock().map_err(|e| e.to_string())?;
        let raw = std::fs::read_to_string(&self.path).map_err(|e| e.to_string())?;
        if cache.as_ref().is_none_or(|(bytes, _)| *bytes != raw) {
            let snapshot = match compat::inspect(raw.as_bytes())? {
                compat::HistoryRead::ReadOnlyV2(_) => {
                    serde_json::from_str::<Snapshot>(&raw).map_err(|e| e.to_string())?
                }
                _ => return Err("V2 registration missing".into()),
            };
            *cache = Some((raw, snapshot));
        }
        apply(&cache.as_ref().ok_or("history cache missing")?.1)
    }
    pub(crate) fn source(&self, namespace: &str, key: &Value) -> Result<Source, String> {
        self.inspect(|state| {
            state
                .locate(namespace, key)
                .and_then(|(r, _)| r.source.clone())
                .ok_or_else(|| "source not bound".into())
        })
    }
    pub(crate) fn registered(
        &self,
        namespace: &str,
        key: &Value,
        path: &Path,
        frame: &str,
    ) -> bool {
        self.inspect(|state| {
            Ok(state.locate(namespace, key).is_some_and(|(r, e)| {
                r.frame.hash == hash(frame)
                    && e.is_some_and(|e| {
                        e.path == path
                            && std::fs::metadata(path).is_ok_and(|m| file_id(&m) == e.file)
                    })
            }))
        })
        .unwrap_or(false)
    }
    pub(crate) fn receipt(
        &self,
        namespace: &str,
        key: &Value,
        (path, offset, kind, row, frame): (&Path, u64, &str, Option<&str>, &str),
    ) -> Option<bool> {
        let file = file_id(&std::fs::metadata(path).ok()?);
        self.transaction(|state| {
            let Some((registration, Some(entry))) = state.locate(namespace, key) else {
                return Ok(None);
            };
            if state.abandoned(namespace, key) {
                return Ok(None);
            }
            if entry.path != path
                || entry.file != file
                || entry.offset > offset
                || registration.frame.hash != hash(frame)
            {
                return Ok(None);
            }
            let event_key =
                row.map_or_else(|| format!("{kind}:{offset}"), |row| format!("{kind}:{row}"));
            let same = |e: &Entry| e.path == path && e.file == file;
            if let Some((owner, _)) = state
                .entries()
                .find(|(_, e)| same(e) && e.events.iter().any(|event| event.key == event_key))
            {
                return Ok(owner
                    .is_some_and(|r| r.namespace == namespace && r.key == *key)
                    .then_some(true));
            }
            let collision =
                collision::contested(state, (namespace, key, registration, entry), kind, offset);
            if collision {
                return Ok(None);
            }
            let event = Event {
                key: event_key,
                row: row.map(str::to_string),
                kind: kind.into(),
            };
            if let Some(bound) = state
                .live
                .iter_mut()
                .find(|b| b.registration.namespace == namespace && b.registration.key == *key)
            {
                bound.entry.events.push(event);
            } else if let Some(passive) = state.passive.iter_mut().find(|p| {
                p.registration
                    .as_ref()
                    .is_some_and(|r| r.namespace == namespace && r.key == *key)
            }) {
                passive
                    .entry
                    .as_mut()
                    .ok_or("unbound passive")?
                    .events
                    .push(event);
                passive.encoded = None;
            }
            Ok(Some(true))
        })
        .ok()
        .flatten()
    }
    pub(crate) fn json(&self) -> Result<Value, String> {
        self.inspect(|state| serde_json::to_value(state).map_err(|e| e.to_string()))
    }
}

#[derive(Clone)]
struct Context {
    registry: Registry,
    namespace: String,
    key: Value,
}
thread_local! { static CONTEXT: std::cell::RefCell<Option<Context>> = const { std::cell::RefCell::new(None) }; }
struct Scope(Option<Context>);
impl Drop for Scope {
    fn drop(&mut self) {
        CONTEXT.with(|slot| *slot.borrow_mut() = self.0.take());
    }
}
fn context() -> Option<Context> {
    CONTEXT.with(|slot| slot.borrow().clone())
}
pub(crate) fn with_attempt<T>(
    registry: &Registry,
    namespace: &str,
    key: &Value,
    f: impl FnOnce() -> T,
) -> Result<T, String> {
    let source = registry.inspect(|state| {
        state
            .locate(namespace, key)
            .map(|(r, _)| r.source.clone())
            .ok_or_else(|| "unregistered attempt scope".into())
    })?;
    if let Some(source) = source {
        source.validate(key)?;
    }
    let previous = CONTEXT.with(|slot| {
        slot.replace(Some(Context {
            registry: registry.clone(),
            namespace: namespace.into(),
            key: key.clone(),
        }))
    });
    let _scope = Scope(previous);
    Ok(f())
}
pub(super) fn scoped_register(
    args: (&str, &str, &str, &str),
    path: &Path,
    offset: u64,
    frame: &str,
) -> Option<Result<(), String>> {
    let c = context()?;
    let (provider, session, nonce, _) = args;
    let registered = c.registry.registered(&c.namespace, &c.key, path, frame);
    let source = c.registry.source(&c.namespace, &c.key);
    Some(source.and_then(|s| {
        (registered
            && c.key["attempt"]["nonce"] == nonce
            && s.provider == provider
            && s.target["owner"] == session
            && offset >= s.offset)
            .then_some(())
            .ok_or_else(|| "adapter registration differs from prepared attempt".into())
    }))
}
pub(super) fn scoped_registered(id: &str, path: &Path, frame: &str) -> Option<bool> {
    let c = context()?;
    Some(
        c.key["attempt"]["nonce"] == id && c.registry.registered(&c.namespace, &c.key, path, frame),
    )
}
pub(super) fn scoped_receipt(
    id: &str,
    path: &Path,
    offset: u64,
    kind: &str,
    row: Option<&str>,
    frame: &str,
) -> Option<Option<bool>> {
    let c = context()?;
    Some(
        (c.key["attempt"]["nonce"] == id)
            .then(|| {
                c.registry
                    .receipt(&c.namespace, &c.key, (path, offset, kind, row, frame))
            })
            .flatten(),
    )
}
pub(super) fn scoped_receipt_row(id: &str, path: &Path) -> Option<Option<String>> {
    let c = context()?;
    Some(
        c.registry
            .inspect(|state| {
                Ok(state
                    .locate(&c.namespace, &c.key)
                    .and_then(|(_, e)| e)
                    .filter(|e| e.id == id && e.path == path)
                    .and_then(|e| e.events.iter().find_map(|event| event.row.clone())))
            })
            .ok()
            .flatten(),
    )
}
pub(super) fn scoped_observed(
    provider: &str,
    session: &str,
    prompts: &[String],
    row: Option<&str>,
) -> Option<ObservedAttempt> {
    let c = context()?;
    Some(
        c.registry
            .inspect(|state| {
                let matching = (!state.abandoned(&c.namespace, &c.key))
                    .then(|| state.locate(&c.namespace, &c.key))
                    .flatten()
                    .and_then(|(_, e)| e)
                    .filter(|e| {
                        e.provider == provider
                            && e.session == session
                            && prompts
                                .iter()
                                .flat_map(|p| class_hashes(p))
                                .any(|hash| hash == e.class_hash)
                    });
                Ok(match matching {
                    Some(e)
                        if row.is_some_and(|row| {
                            e.events
                                .iter()
                                .any(|event| event.row.as_deref() == Some(row))
                        }) =>
                    {
                        ObservedAttempt::Known(e.id.clone())
                    }
                    Some(_) => ObservedAttempt::Pending,
                    None => ObservedAttempt::None,
                })
            })
            .unwrap_or(ObservedAttempt::Pending),
    )
}
pub(super) fn scoped() -> bool {
    context().is_some()
}

// Writer fixtures require Unix file identity and confirmed parent directory fsync.
#[cfg(unix)]
#[path = "v2_tests.rs"]
mod tests;

pub(crate) fn scoped_source() -> Option<Result<Source, String>> {
    let c = context()?;
    Some(c.registry.source(&c.namespace, &c.key).and_then(|source| {
        source.validate(&c.key)?;
        Ok(source)
    }))
}
