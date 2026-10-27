//! Consumes an Unknown head only when its provider's bound native transcript proves it was taken.
//! Claude's legacy proof still requires the exact enqueue, absorbed removal and human command.
#[cfg(test)]
pub(super) fn v2_record_matches(
    attempt: &serde_json::Value,
    target: &super::Target,
    provider: super::queued_provider::QueuedProvider,
    kind: &crate::services::provider::ProviderKind,
) -> bool {
    let Ok(meta) = std::fs::metadata(&target.transcript) else {
        return false;
    };
    let Some((dev, ino)) = file_id(&meta) else {
        return false;
    };
    let bound = target
        .binding
        .as_ref()
        .and_then(|binding| binding.4.as_deref());
    let Some(session) = provider.session(&target.transcript, bound) else {
        return false;
    };
    let source = &attempt["source"];
    attempt["provider"] == kind.as_str()
        && attempt["target"]["owner"] == target.session
        && source["kind"] == "existing"
        && source["session_id"] == session
        && source["transcript"] == target.transcript.display().to_string()
        && source["file"] == serde_json::json!([dev, ino])
        && source["offset"]
            .as_u64()
            .is_some_and(|offset| offset <= meta.len())
}
#[cfg(test)]
impl crate::services::turn_orchestrator::ChannelMailboxHandle {
    pub(crate) async fn v2_settle_exact(
        &self,
        identity: serde_json::Value,
        delivered: bool,
        now: u64,
    ) -> Result<(), String> {
        self.v2_request(move |slot| {
            let key = serde_json::from_value(identity).map_err(|e| e.to_string())?;
            slot.as_mut()
                .ok_or("V2 not adopted")?
                .settle(&key, delivered, now)
        })
        .await
    }
}
use std::collections::{BTreeSet, HashMap};
use std::io::{BufRead, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::queued_provider::QueuedProvider;
use super::queued_support::RECONCILED;
use super::{HumanInputRequest, InjectAttempt, SharedData, channel_name, live_tui_session};
use crate::services::claude_tui::busy_inject;
use crate::services::tui_input::human_frame::{self, Mode};
use crate::services::turn_orchestrator::{
    PendingQueueItem, QueueIdentity, SourceMessageQueuedGeneration,
};
use poise::serenity_prelude::ChannelId;

type FileId = (u64, u64);

#[cfg(unix)]
fn file_id(meta: &std::fs::Metadata) -> Option<FileId> {
    use std::os::unix::fs::MetadataExt;
    Some((meta.dev(), meta.ino()))
}

#[cfg(not(unix))]
fn file_id(_: &std::fs::Metadata) -> Option<FileId> {
    None
}

/// Consumes an Unknown head on provider proof alone, without pane input.
/// Without that proof, the ordinary owner still sees the Unknown barrier.
pub(super) async fn mend(
    shared: &Arc<SharedData>,
    request: &HumanInputRequest,
    parent: Option<ChannelId>,
    gate: fn(ChannelId, Option<ChannelId>) -> bool,
) -> Option<InjectAttempt> {
    let channel = request.channel_id;
    let mailbox = shared.mailbox(channel);
    let discord = crate::services::discord::queue_persistence_context;
    let persistence = discord(shared, &request.provider, channel);
    mailbox.unknown_queued_head(persistence.clone()).await?;
    // Held from the head and transcript read to the commit, as a paste's owner holds it.
    let transition = shared.session_transition_lock(channel).try_lock_owned();
    let _transition = transition.ok()?;
    let head = mailbox.unknown_queued_head(persistence.clone()).await?;
    let bound = bound_transcript(shared, request).await?;
    let recorded = head.attempt.as_ref().map(|attempt| Recorded {
        nonce: attempt.nonce.clone(),
        transcript: attempt.transcript.clone(),
        session_id: attempt.session_id.clone(),
        file: attempt.file,
        offset: attempt.offset,
    });
    let provider = QueuedProvider::select(&request.provider, channel.get())?;
    let (identity, original, transcript) =
        (head.identity.clone(), head.original.clone(), bound.clone());
    let proof = tokio::task::spawn_blocking(move || {
        provider.proof(channel, &identity, &original, recorded, &transcript)
    })
    .await
    .ok()??;
    #[cfg(test)]
    super::test_hook::before_queued(channel.get(), "mend_commit").await;
    if bound_transcript(shared, request).await.as_ref() != Some(&bound) {
        return None;
    }
    let (primary, generation) = (head.identity.primary, head.identity.queued_generation);
    let recorded_attempt = head.attempt.is_some();
    let sources: Vec<_> = head
        .identity
        .sources
        .iter()
        .map(|source| {
            SourceMessageQueuedGeneration::new(
                poise::serenity_prelude::MessageId::new(source.message_id),
                source.queued_generation,
            )
        })
        .collect();
    let admission = (channel, parent, gate);
    if let Err(error) = mailbox.mend_queued_head(head, persistence, admission).await {
        let channel_id = channel.get();
        tracing::warn!(channel_id, primary, %error, "queued head proof was not applied");
        return None;
    }
    drop(_transition);
    super::catch_up_race::confirmed(shared, &request.provider, channel, &sources).await;
    tracing::info!(
        channel_id = channel.get(),
        head_message_id = primary,
        queued_generation = generation,
        nonce = %proof.nonce,
        recorded_attempt,
        native_session = %proof.session,
        enqueue_offset = proof.offset,
        "queued_head_reconciled"
    );
    Some(InjectAttempt::NotSent(RECONCILED))
}

/// The native transcript the channel's live provider pane is bound to now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Bound {
    pub(super) session: String,
    pub(super) path: PathBuf,
    pub(super) file: FileId,
    pub(super) session_id: String,
}

async fn bound_transcript(shared: &SharedData, request: &HumanInputRequest) -> Option<Bound> {
    let read = crate::services::discord::inflight::load_inflight_state_read_only;
    let row = read(&request.provider, request.channel_id.get());
    let named = channel_name(shared, request.channel_id).await;
    let provider = QueuedProvider::select(&request.provider, request.channel_id.get())?;
    let runtime = provider.runtime();
    let (session, path) = live_tui_session(shared, request, runtime, row.as_ref(), named)?;
    let binding = crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(&session)?;
    let session_id = provider.session(&path, binding.session_id.as_deref())?;
    let file = file_id(&std::fs::metadata(&path).ok()?)?;
    Some(Bound {
        session,
        path,
        file,
        session_id,
    })
}

/// The Claude session a transcript is named for, `<uuid>.jsonl`; a binding that names a session
/// must name the same one.
pub(super) fn transcript_session(path: &Path, bound: Option<&str>) -> Option<String> {
    let jsonl = path.extension().is_some_and(|ext| ext == "jsonl");
    let stem = path.file_stem()?.to_str().filter(|_| jsonl)?;
    uuid::Uuid::parse_str(stem).ok()?;
    bound
        .is_none_or(|bound| bound == stem)
        .then(|| stem.to_string())
}

/// The paste a head recorded before it could start.
pub(super) struct Recorded {
    pub(super) nonce: String,
    pub(super) transcript: String,
    pub(super) session_id: String,
    pub(super) file: FileId,
    pub(super) offset: u64,
}

/// Where a search may resume per transcript and head: no line before it names the head's source.
type ScanKey = (PathBuf, FileId, u64, u64);
static RESUME: Mutex<Option<HashMap<ScanKey, u64>>> = Mutex::new(None);

/// What proves this head: its frame under one nonce, the recorded one when the head names its
/// paste, else the only one enqueued after the source's own enqueue.
struct Want {
    key: ScanKey,
    from: u64,
    /// The pre-v1 frame up to its nonce.
    lead: String,
    source: String,
    author: String,
    text: String,
    nonce: Option<String>,
    session_id: String,
    after_us: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Kind {
    Enqueue,
    /// An absorbed removal, by its command and delivery ids.
    Removed(String, String),
    /// A human prompt queued command, by the ids of the removal it came from.
    Command(String, String),
}

struct Seen {
    kind: Kind,
    nonce: String,
    body: String,
    session: String,
    offset: u64,
    stamp_us: Option<i64>,
}

pub(super) struct Proof {
    pub(super) nonce: String,
    pub(super) session: String,
    pub(super) offset: u64,
}

impl Want {
    fn of(
        channel: ChannelId,
        identity: &QueueIdentity,
        original: &PendingQueueItem,
        recorded: Option<Recorded>,
        bound: &Bound,
    ) -> Option<Self> {
        let (nonce, from, after_us) = match recorded {
            // Only the paste's own transcript file and session can hold its record.
            Some(recorded) => {
                if !recorded.matches(bound) {
                    return None;
                }
                (Some(recorded.nonce), recorded.offset, None)
            }
            // A head recorded before attempts were: one source, pasted after its enqueue clock.
            None => {
                let [source] = identity.sources.as_slice() else {
                    return None;
                };
                let clock = (original.source_message_queued_generations.iter())
                    .filter(|clock| clock.message_id == source.message_id)
                    .find(|clock| clock.queued_generation == source.queued_generation)?;
                let after = i64::try_from(clock.enqueued_at_epoch_us?).ok()?;
                (None, 0, Some(after))
            }
        };
        let source = format!("discord:{channel}:{}", original.message_id);
        let author = original.author_id.to_string();
        let lead = human_frame::legacy_inject(&source, &author, "", "");
        let key = (
            bound.path.clone(),
            bound.file,
            identity.primary,
            identity.queued_generation,
        );
        Some(Self {
            key,
            from,
            lead: lead.strip_suffix("]\n")?.to_string(),
            source,
            author,
            text: original.text.clone(),
            nonce,
            session_id: bound.session_id.clone(),
            after_us,
        })
    }

    fn search(self) -> Option<Proof> {
        let mut file = std::fs::File::open(&self.key.0).ok()?;
        let meta = file
            .metadata()
            .ok()
            .filter(|m| file_id(m) == Some(self.key.1))?;
        let resume = {
            let resume = RESUME.lock().unwrap_or_else(|e| e.into_inner());
            resume.as_ref().and_then(|map| map.get(&self.key).copied())
        };
        let start = resume.filter(|at| *at <= meta.len()).unwrap_or(0);
        let start = start.max(self.from);
        file.seek(SeekFrom::Start(start)).ok()?;
        let mut reader = std::io::BufReader::new(file);
        let (mut offset, mut first, mut line, mut seen) = (start, None, Vec::new(), Vec::new());
        // A line without its newline may still be mid-write, so the search stops before it.
        while reader.read_until(b'\n', &mut line).ok()? > 0 && line.ends_with(b"\n") {
            let text = std::str::from_utf8(&line).unwrap_or_default();
            if self.nonce.is_some() || text.contains(&self.source) {
                first.get_or_insert(offset);
                seen.extend(self.record(text, offset));
            }
            offset += line.len() as u64;
            line.clear();
        }
        let mut resume = RESUME.lock().unwrap_or_else(|e| e.into_inner());
        let resume = resume.get_or_insert_with(HashMap::new);
        resume.insert(self.key.clone(), first.unwrap_or(offset));
        self.decide(&seen)
    }

    /// An enqueue, an absorbed removal or a human prompt command whose body is this head's frame.
    fn record(&self, line: &str, offset: u64) -> Option<Seen> {
        use serde_json::Value;
        let record: Value = serde_json::from_str(line).ok()?;
        let text = |value: &Value, key: &str| value.get(key)?.as_str().map(str::to_string);
        let ids = |value: &Value, command: &str, delivery: &str| {
            let ids = (text(value, command)?, text(value, delivery)?);
            Some(ids).filter(|(command, delivery)| !command.is_empty() && !delivery.is_empty())
        };
        let (kind, body) = match record.get("type")?.as_str()? {
            "queue-operation" => match record.get("operation")?.as_str()? {
                "enqueue" => (Kind::Enqueue, text(&record, "content")?),
                "remove" if text(&record, "reason")? == "absorbed_mid_turn" => {
                    let (command, delivery) = ids(&record, "commandUuid", "deliveryId")?;
                    (Kind::Removed(command, delivery), text(&record, "content")?)
                }
                _ => return None,
            },
            "attachment" => {
                let command = record.get("attachment")?;
                let human = text(command, "type")? == "queued_command"
                    && text(command, "commandMode")? == "prompt"
                    && text(command.get("origin")?, "kind")? == "human"
                    && command.get("humanTurn")?.as_bool()?;
                let ids = ids(command, "source_uuid", "delivery_id").filter(|_| human)?;
                (Kind::Command(ids.0, ids.1), text(command, "prompt")?)
            }
            _ => return None,
        };
        let stamp = text(&record, "timestamp");
        let stamp = stamp.and_then(|at| chrono::DateTime::parse_from_rfc3339(&at).ok());
        Some(Seen {
            nonce: self.nonce_in(&body)?,
            kind,
            body,
            session: text(&record, "sessionId").filter(|id| !id.is_empty())?,
            offset,
            stamp_us: stamp.map(|at| at.timestamp_micros()),
        })
    }

    /// The nonce of the frame `body` is exactly, bare or in one paste block: the v1 frame this
    /// head's source and author rebuild under the name it shows, or the frame pasted before v1.
    fn nonce_in(&self, body: &str) -> Option<String> {
        let header = body
            .find("[User: ")
            .and_then(|at| human_frame::parse(&body[at..]));
        if let Some((header, _)) = header.filter(|(header, _)| header.mode == Mode::Inject) {
            let (name, nonce) = (header.name, header.nonce);
            if nonce.is_empty() {
                let id = self.nonce.as_deref()?;
                let frame = human_frame::idle(name, &self.author, &self.text);
                if human_frame::history::registered(id, &self.key.0, &frame)
                    && human_frame::history::unique_attempt(id, &self.key.0, &frame)
                    && busy_inject::exact_receipt(body, &frame)
                {
                    return Some(id.to_string());
                }
                return None;
            }
            let frame = human_frame::inject(&self.source, &self.author, name, nonce, &self.text);
            if header.author == self.author && busy_inject::exact_receipt(body, &frame) {
                return Some(nonce.to_string());
            }
        }
        let at = body.find(&self.lead)? + self.lead.len();
        let nonce = body.get(at..at + 8)?;
        let hex = nonce
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        let frame = human_frame::legacy_inject(&self.source, &self.author, nonce, &self.text);
        (hex && busy_inject::exact_receipt(body, &frame)).then(|| nonce.to_string())
    }

    /// One enqueue under the nonce in the bound session, then an absorbed removal and, after it, a
    /// human command naming the same ids, each with the enqueue's body and session.
    fn decide(&self, seen: &[Seen]) -> Option<Proof> {
        let paste = |seen: &Seen| {
            let after = |after: i64| seen.stamp_us.is_some_and(|at| at >= after);
            seen.kind == Kind::Enqueue && self.after_us.is_none_or(after)
        };
        let nonce = match &self.nonce {
            Some(nonce) => nonce.clone(),
            None => {
                let pasted = seen.iter().filter(|seen| paste(seen));
                let nonces: BTreeSet<&str> = pasted.map(|seen| seen.nonce.as_str()).collect();
                let [only] = nonces.into_iter().collect::<Vec<_>>()[..] else {
                    return None;
                };
                only.to_string()
            }
        };
        let mine: Vec<&Seen> = seen.iter().filter(|seen| seen.nonce == nonce).collect();
        let enqueues: Vec<&Seen> = (mine.iter().copied())
            .filter(|seen| seen.kind == Kind::Enqueue)
            .collect();
        let [enqueue] = enqueues[..] else {
            return None;
        };
        let bound_session = self.session_id == enqueue.session;
        let after = mine.iter().copied().filter(|seen| {
            seen.offset > enqueue.offset
                && seen.body == enqueue.body
                && seen.session == enqueue.session
        });
        // In file order: a command counts only once its removal has been seen.
        let mut removed: Vec<(&str, &str)> = Vec::new();
        let linked = after.into_iter().any(|seen| match &seen.kind {
            Kind::Removed(command, delivery) => {
                removed.push((command, delivery));
                false
            }
            Kind::Command(command, delivery) => removed.contains(&(command, delivery)),
            Kind::Enqueue => false,
        });
        (paste(enqueue) && bound_session && linked).then(|| Proof {
            nonce,
            session: enqueue.session.clone(),
            offset: enqueue.offset,
        })
    }
}

impl Recorded {
    pub(super) fn matches(&self, bound: &Bound) -> bool {
        self.transcript == bound.path.display().to_string()
            && self.file == bound.file
            && self.session_id == bound.session_id
    }
}

pub(super) fn claude_proof(
    channel: ChannelId,
    identity: &QueueIdentity,
    original: &PendingQueueItem,
    recorded: Option<Recorded>,
    bound: &Bound,
) -> Option<Proof> {
    Want::of(channel, identity, original, recorded, bound)?.search()
}

#[cfg(test)]
#[path = "queued_reconcile_tests.rs"]
mod tests;

#[cfg(test)]
#[test]
fn mismatched_file_identity_refuses_transcript_proof() {
    use serde_json::json;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("transcript.jsonl");
    let source = "discord:6813460:9100094081725135881";
    let session = "47d7dbe8-0c1a-4b2e-9f3d-6a5b4c3d2e1f";
    let nonce = "d286a43b";
    let body = human_frame::inject(source, "200", "ann", nonce, "status?");
    let records = [
        json!({"type": "queue-operation", "operation": "enqueue",
            "sessionId": session, "content": body}),
        json!({"type": "queue-operation", "operation": "remove",
            "sessionId": session, "content": body, "reason": "absorbed_mid_turn",
            "commandUuid": "cmd", "deliveryId": "del"}),
        json!({"type": "attachment", "sessionId": session,
            "attachment": {"type": "queued_command", "prompt": body,
                "source_uuid": "cmd", "delivery_id": "del", "commandMode": "prompt",
                "origin": {"kind": "human"}, "humanTurn": true}}),
    ];
    let transcript = records.map(|record| format!("{record}\n")).concat();
    std::fs::write(&path, &transcript).unwrap();
    let actual = file_id(&std::fs::metadata(&path).unwrap());
    let different = actual
        .map(|(device, inode)| (device, inode.wrapping_add(1)))
        .unwrap_or((0, 0));
    assert_ne!(actual, Some(different));
    let want = Want {
        key: (path.clone(), different, 1, 1),
        from: 0,
        lead: String::new(),
        source: source.into(),
        author: "200".into(),
        text: "status?".into(),
        nonce: Some(nonce.into()),
        session_id: session.into(),
        after_us: None,
    };
    let mut offset = 0;
    let seen: Vec<_> = transcript
        .lines()
        .filter_map(|line| {
            let record = want.record(line, offset);
            offset += line.len() as u64 + 1;
            record
        })
        .collect();
    assert_eq!(seen.len(), 3);
    assert_eq!(want.decide(&seen).unwrap().nonce, nonce);
    assert!(
        want.search().is_none(),
        "a complete proof chain cannot authorize a different transcript identity"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), transcript);
}
