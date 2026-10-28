//! Exact-incarnation handoff orchestration; callers supply evidence before every effect.

use crate::services::discord::runtime_store;
use crate::services::tui_input::actor::gate::{PaneVerdict, judge_pane};
use crate::services::tui_o::shadow::ShadowProvider;
use crate::services::tui_prompt_dedupe::binding_events::SourceId;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Owner {
    pub runtime_root: PathBuf,
    pub provider: String,
    pub token_hash: String,
    pub channel: u64,
    pub logical_key: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Revision {
    pub clear_generation: u64,
    pub cached_sid: Option<String>,
    pub raw_sid: Option<String>,
    pub cwd: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SourceFence {
    pub source: SourceId,
    pub prefix_eof: u64,
    pub prefix_sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OldIncarnation {
    pub nonce: String,
    pub physical_pane: [String; 5],
    pub provider_pid: u32,
    pub starttime: Option<u128>,
    pub macos_lstart_hash: Option<u128>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Intent {
    pub owner: Owner,
    pub canonical_row: i64,
    pub hosted_raw: Option<String>,
    pub revision: Revision,
    pub source: SourceFence,
    pub old: OldIncarnation,
    pub request_key: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Hosted {
    Legacy,
    Pending(String),
    Bound(String),
    Unknown,
}

pub(super) struct Observation {
    pub owner: Owner,
    pub canonical_row: i64,
    pub hosted_raw: Option<String>,
    pub revision: Revision,
    pub source: SourceFence,
    pub request_key: String,
    pub marker_nonce: String,
    pub source_exclusive: bool,
    pub token_live: bool,
    pub o_ready: bool,
    pub prior_responsibility_drained: bool,
    pub hosted: Hosted,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum OldProbe {
    Alive(Box<OldIncarnation>, String),
    AbsentNoReplacement,
    Replaced,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MigrationControl {
    Exit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Effect {
    Exit,
    Prepare,
    Pending,
    Create,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Cas {
    Written,
    Stale,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Create {
    Acknowledged,
    Unknown,
    NotSent,
}

pub(super) trait Host {
    /// Source evidence includes a freshly verified prefix, complete EOF, and exact delivery debt.
    fn observe(&mut self, intent: &Intent, effect: Effect) -> Result<Observation, String>;
    fn probe_old(&mut self, intent: &Intent) -> Result<OldProbe, String>;
    /// The short transport fence invokes `prewrite` after its last wait and before any byte.
    fn control(
        &mut self,
        intent: &Intent,
        control: MigrationControl,
        prewrite: impl FnOnce(&mut Self) -> Result<(), String>,
    ) -> Result<(), String>;
    /// Repeated preparation of this nonce must reuse its immutable context and exact source.
    fn prepare(&mut self, intent: &Intent, nonce: &str) -> Result<(), String>;
    fn install_pending(&mut self, intent: &Intent, nonce: &str) -> Result<Cas, String>;
    fn create(&mut self, intent: &Intent, nonce: &str) -> Result<Create, String>;
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Invalidation {
    Clear,
    Resume,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
enum Phase {
    ExitMayHaveBeenSent,
    OldAbsent { nonce: String },
    Pending { nonce: String },
    Invalidated { cause: Invalidation },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    schema: u8,
    intent: Intent,
    state: Phase,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Outcome {
    Created(String),
    ExistingPending(String),
    AlreadyBound(String),
    WaitingForOldExit,
    Hold(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Write,
    FileSync,
    Rename,
    ParentSync,
    MigrationSync,
    RootSync,
}

#[derive(Default)]
struct PublicationTrace {
    phase: Option<Phase>,
    operations: Vec<(Step, bool)>,
}

pub(crate) struct Store {
    root: PathBuf,
    dir: PathBuf,
    owner: Owner,
    fail: Option<Step>,
    publication: Arc<Mutex<PublicationTrace>>,
}

pub(crate) struct Lock(File);
impl Drop for Lock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

fn safe_dir(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => runtime_store::fsync_parent_dir(path)?,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    if !fs::symlink_metadata(path)?.is_dir() {
        return Err(io::Error::other("unsafe directory"));
    }
    Ok(())
}

impl Store {
    pub(crate) fn open(root: &Path, owner: &Owner) -> io::Result<Self> {
        if owner.provider != "claude"
            || owner.channel == 0
            || owner.logical_key.is_empty()
            || owner.token_hash.is_empty()
            || root.canonicalize()? != owner.runtime_root
        {
            return Err(io::Error::other("owner/root mismatch"));
        }
        let root = root.canonicalize()?;
        let parent = root.join("claude_migration");
        safe_dir(&parent)?;
        use sha2::{Digest, Sha256};
        let identity = serde_json::to_vec(&(&root, &owner.logical_key))?;
        let dir = parent.join(format!("{:x}", Sha256::digest(identity)));
        safe_dir(&dir)?;
        Ok(Self {
            root,
            dir,
            owner: owner.clone(),
            fail: None,
            publication: Arc::new(Mutex::new(PublicationTrace::default())),
        })
    }

    pub(crate) fn direct_effect_permit(&self) -> io::Result<Lock> {
        self.lock("effect.lock")
    }

    fn lock(&self, name: &str) -> io::Result<Lock> {
        let path = self.dir.join(name);
        if fs::symlink_metadata(&path).is_ok_and(|m| !m.is_file() || m.file_type().is_symlink()) {
            return Err(io::Error::other("unsafe lock file"));
        }
        use std::os::unix::fs::OpenOptionsExt;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .custom_flags(libc::O_NOFOLLOW)
            .mode(0o600)
            .open(&path)?;
        file.try_lock().map_err(io::Error::from)?;
        use std::os::unix::fs::MetadataExt;
        let pinned = file.metadata()?;
        let current = fs::symlink_metadata(path)?;
        if !current.is_file() || current.dev() != pinned.dev() || current.ino() != pinned.ino() {
            let _ = file.unlock();
            return Err(io::Error::other("lock path replaced"));
        }
        Ok(Lock(file))
    }

    fn read(&self) -> io::Result<Option<Receipt>> {
        let path = self.dir.join("receipt.json");
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        if !metadata.is_file() || metadata.len() > 64 * 1024 {
            return Err(io::Error::other("unsafe receipt"));
        }
        let receipt: Receipt = serde_json::from_slice(&fs::read(path)?)?;
        if receipt.schema != 1 {
            return Err(io::Error::other("unknown receipt schema"));
        }
        Ok(Some(receipt))
    }

    fn operation(&self, step: Step, work: impl FnOnce() -> io::Result<()>) -> io::Result<()> {
        let result = if self.fail == Some(step) {
            Err(io::Error::other(format!("injected {step:?}")))
        } else {
            work()
        };
        self.publication
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .operations
            .push((step, result.is_ok()));
        result
    }

    fn publish(&self, intent: &Intent, state: Phase) -> io::Result<()> {
        let receipt = Receipt {
            schema: 1,
            intent: intent.clone(),
            state,
        };
        let bytes = serde_json::to_vec(&receipt)?;
        if bytes.len() > 64 * 1024 {
            return Err(io::Error::other("oversized receipt"));
        }
        *self
            .publication
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = PublicationTrace {
            phase: Some(receipt.state),
            operations: Vec::new(),
        };
        let path = self.dir.join("receipt.json");
        let tmp = self.dir.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&tmp)?;
        self.operation(Step::Write, || file.write_all(&bytes))?;
        self.operation(Step::FileSync, || file.sync_all())?;
        self.operation(Step::Rename, || fs::rename(tmp, &path))?;
        self.operation(Step::ParentSync, || runtime_store::fsync_parent_dir(&path))?;
        self.operation(Step::MigrationSync, || {
            runtime_store::fsync_parent_dir(&self.dir)
        })?;
        self.operation(Step::RootSync, || {
            runtime_store::fsync_parent_dir(
                self.dir
                    .parent()
                    .ok_or_else(|| io::Error::other("no receipt parent"))?,
            )
        })
    }

    /// Invalidation and the short selector/revision commit share one gate, including absent receipts.
    pub(crate) fn with_selector_change<R>(
        &self,
        cause: Invalidation,
        commit: impl FnOnce() -> Result<R, String>,
    ) -> Result<R, String> {
        let _gate = self.lock("effect.lock").map_err(|e| e.to_string())?;
        if let Some(receipt) = self.read().map_err(|e| e.to_string())? {
            if receipt.intent.owner != self.owner {
                return Err("handoff receipt belongs to another owner".into());
            }
            self.publish(&receipt.intent, Phase::Invalidated { cause })
                .map_err(|e| e.to_string())?;
        }
        commit()
    }
}

fn exact_intent(intent: &Intent) -> bool {
    intent.canonical_row > 0
        && !intent.request_key.is_empty()
        && uuid::Uuid::parse_str(&intent.old.nonce).is_ok()
        && intent.old.provider_pid > 0
        && (intent.old.starttime.is_some() || intent.old.macos_lstart_hash.is_some())
        && intent.old.physical_pane.iter().all(|s| !s.is_empty())
        && uuid::Uuid::parse_str(&intent.source.source.session_id).is_ok()
        && intent.source.source.path.is_absolute()
        && intent.source.source.dev > 0
        && intent.source.source.ino > 0
        && intent.source.prefix_sha256.len() == 64
        && intent
            .source
            .prefix_sha256
            .bytes()
            .all(|b| b.is_ascii_hexdigit())
}

fn verify_source(fence: &SourceFence) -> Result<(), String> {
    fn inspect(fence: &SourceFence) -> io::Result<()> {
        use sha2::{Digest, Sha256};
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        let path = &fence.source.path;
        if !fs::symlink_metadata(path)?.is_file() {
            return Err(io::Error::other("source path changed"));
        }
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        let before = file.metadata()?;
        if before.dev() != fence.source.dev
            || before.ino() != fence.source.ino
            || before.len() < fence.prefix_eof
        {
            return Err(io::Error::other("source identity changed or shrank"));
        }
        let mut reader = BufReader::new(file);
        let mut digest = Sha256::new();
        let mut offset = 0u64;
        let mut line = Vec::new();
        loop {
            line.clear();
            if reader.read_until(b'\n', &mut line)? == 0 {
                break;
            }
            if line.last() != Some(&b'\n')
                || serde_json::from_slice::<serde_json::Value>(&line).is_err()
            {
                return Err(io::Error::other("source EOF or record is incomplete"));
            }
            offset += line.len() as u64;
            if offset <= fence.prefix_eof {
                digest.update(&line);
            } else if offset - (line.len() as u64) < fence.prefix_eof {
                return Err(io::Error::other("prefix EOF is not a record boundary"));
            }
        }
        let after = reader.get_ref().metadata()?;
        let current = fs::symlink_metadata(path)?;
        if format!("{:x}", digest.finalize()) != fence.prefix_sha256.to_ascii_lowercase()
            || offset != before.len()
            || after.len() != before.len()
            || after.modified()? != before.modified()?
            || current.modified()? != before.modified()?
            || current.dev() != before.dev()
            || current.ino() != before.ino()
            || current.len() != before.len()
        {
            return Err(io::Error::other(
                "source prefix or identity changed while reading",
            ));
        }
        Ok(())
    }
    inspect(fence).map_err(|e| format!("source fence: {e}"))
}

fn validate(intent: &Intent, observed: &Observation, expected: &Hosted) -> Result<(), String> {
    let nonce = match expected {
        Hosted::Pending(nonce) | Hosted::Bound(nonce) => nonce,
        _ => &intent.old.nonce,
    };
    if observed.marker_nonce != *nonce
        || !observed.source_exclusive
        || observed.owner != intent.owner
        || observed.canonical_row != intent.canonical_row
        || observed.revision != intent.revision
        || observed.source != intent.source
        || observed.request_key != intent.request_key
        || !observed.token_live
        || !observed.o_ready
        || !observed.prior_responsibility_drained
        || &observed.hosted != expected
        || (matches!(expected, Hosted::Legacy) && observed.hosted_raw != intent.hosted_raw)
    {
        return Err("migration readset/token/drain changed".into());
    }
    verify_source(&intent.source)
}

fn successor_nonce(intent: &Intent, nonce: &str) -> bool {
    matches!((uuid::Uuid::parse_str(nonce), uuid::Uuid::parse_str(&intent.old.nonce)),
        (Ok(next), Ok(old)) if next != old)
}

fn ready_old(intent: &Intent, probe: OldProbe) -> Result<(), String> {
    match probe {
        OldProbe::Alive(old, pane)
            if *old == intent.old
                && judge_pane(ShadowProvider::Claude, &pane) == PaneVerdict::Ready =>
        {
            Ok(())
        }
        _ => Err("exact empty ready old incarnation not established".into()),
    }
}

fn still_valid(store: &Store, intent: &Intent) -> Result<(), String> {
    match store.read().map_err(|e| e.to_string())? {
        Some(receipt)
            if receipt.intent != *intent || matches!(receipt.state, Phase::Invalidated { .. }) =>
        {
            Err("migration intent invalidated or replaced".into())
        }
        _ => Ok(()),
    }
}

pub(super) fn drive(store: &Store, intent: &Intent, host: &mut impl Host) -> Outcome {
    match drive_inner(store, intent, host) {
        Ok(outcome) => outcome,
        Err(reason) => Outcome::Hold(reason),
    }
}

fn drive_inner(store: &Store, intent: &Intent, host: &mut impl Host) -> Result<Outcome, String> {
    let _handoff = store.lock("handoff.lock").map_err(|e| e.to_string())?;
    if !exact_intent(intent)
        || intent.owner.runtime_root != store.root
        || intent.owner != store.owner
    {
        return Err("incomplete migration intent".into());
    }
    let saved = store.read().map_err(|e| e.to_string())?;
    if saved
        .as_ref()
        .is_some_and(|r| r.intent != *intent || matches!(r.state, Phase::Invalidated { .. }))
    {
        return Err("migration receipt invalidated or disagrees".into());
    }
    if let Some(Phase::OldAbsent { nonce } | Phase::Pending { nonce }) =
        saved.as_ref().map(|r| &r.state)
        && !successor_nonce(intent, nonce)
    {
        return Err("invalid receipt successor nonce".into());
    }
    let initial = host.observe(intent, Effect::Exit)?;
    match &initial.hosted {
        Hosted::Pending(nonce) | Hosted::Bound(nonce) => {
            let expected_nonce = match saved.as_ref().map(|r| &r.state) {
                Some(Phase::OldAbsent { nonce }) | Some(Phase::Pending { nonce }) => Some(nonce),
                _ => None,
            };
            if expected_nonce != Some(nonce) {
                return Err("canonical successor nonce disagrees with receipt".into());
            }
            validate(intent, &initial, &initial.hosted)?;
            return Ok(if matches!(initial.hosted, Hosted::Bound(_)) {
                Outcome::AlreadyBound(nonce.clone())
            } else {
                Outcome::ExistingPending(nonce.clone())
            });
        }
        Hosted::Unknown => return Err("hosted execution unknown".into()),
        Hosted::Legacy => validate(intent, &initial, &Hosted::Legacy)?,
    }
    if saved
        .as_ref()
        .is_some_and(|r| matches!(r.state, Phase::Pending { .. }))
    {
        return Err("pending receipt without matching canonical row".into());
    }
    if saved.is_none() {
        let _gate = store.lock("effect.lock").map_err(|e| e.to_string())?;
        still_valid(store, intent)?;
        validate(
            intent,
            &host.observe(intent, Effect::Exit)?,
            &Hosted::Legacy,
        )?;
        ready_old(intent, host.probe_old(intent)?)?;
        store
            .publish(intent, Phase::ExitMayHaveBeenSent)
            .map_err(|e| e.to_string())?;
        host.control(intent, MigrationControl::Exit, |host| {
            still_valid(store, intent)?;
            validate(
                intent,
                &host.observe(intent, Effect::Exit)?,
                &Hosted::Legacy,
            )?;
            ready_old(intent, host.probe_old(intent)?)
        })?;
    }
    match host.probe_old(intent)? {
        OldProbe::AbsentNoReplacement => {}
        OldProbe::Alive(_, _) => return Ok(Outcome::WaitingForOldExit),
        OldProbe::Replaced | OldProbe::Unknown => {
            return Err("old death or replacement uncertain".into());
        }
    }
    let _gate = store.lock("effect.lock").map_err(|e| e.to_string())?;
    still_valid(store, intent)?;
    validate(
        intent,
        &host.observe(intent, Effect::Prepare)?,
        &Hosted::Legacy,
    )?;
    if host.probe_old(intent)? != OldProbe::AbsentNoReplacement {
        return Err("old absence changed before prepare".into());
    }
    let nonce = match saved.as_ref().map(|r| &r.state) {
        Some(Phase::OldAbsent { nonce }) => nonce.clone(),
        _ => uuid::Uuid::new_v4().simple().to_string(),
    };
    if !successor_nonce(intent, &nonce) {
        return Err("invalid successor nonce".into());
    }
    store
        .publish(
            intent,
            Phase::OldAbsent {
                nonce: nonce.clone(),
            },
        )
        .map_err(|e| e.to_string())?;
    host.prepare(intent, &nonce)?;
    validate(
        intent,
        &host.observe(intent, Effect::Pending)?,
        &Hosted::Legacy,
    )?;
    if host.probe_old(intent)? != OldProbe::AbsentNoReplacement {
        return Err("old absence changed before Pending".into());
    }
    match host.install_pending(intent, &nonce)? {
        Cas::Written => {}
        Cas::Stale | Cas::Unknown => return Err("pending CAS not confirmed".into()),
    }
    store
        .publish(
            intent,
            Phase::Pending {
                nonce: nonce.clone(),
            },
        )
        .map_err(|e| e.to_string())?;
    validate(
        intent,
        &host.observe(intent, Effect::Create)?,
        &Hosted::Pending(nonce.clone()),
    )?;
    if host.probe_old(intent)? != OldProbe::AbsentNoReplacement {
        return Err("old absence changed before create".into());
    }
    match host.create(intent, &nonce)? {
        Create::Acknowledged => Ok(Outcome::Created(nonce)),
        Create::Unknown | Create::NotSent => {
            Err("create outcome requires existing Pending reconciliation".into())
        }
    }
}

#[path = "legacy_handoff_tests.rs"]
mod tests;
