//! Persistent state of the O writer under `<runtime_root>/o_store`: once-written `init` and
//! `o_era`, the delivery ledger, and the raw spool with cursors. Damage halts; it never re-inits.

mod durable;
pub mod ledger;
pub mod rotation;
pub mod spool;

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::shadow::SourceId;
use crate::services::discord::runtime_store::PARENT_DIR_FSYNC_FLUSHES;
use ledger::{LedgerEntry, LedgerState};

pub const STORE_DIR_NAME: &str = "o_store";
pub const ERA_FILE: &str = "o_era";
pub const INIT_FILE: &str = "init";
pub const LEDGER_FILE: &str = "ledger.jsonl";
pub const SPOOL_DIR: &str = "spool";
pub const CURSOR_DIR: &str = "cursor";

/// Derived per channel from its boot ownership, never read from config; no store is created unless
/// enabled.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StoreConfig {
    pub enabled: bool,
}

/// A source bound at the switch; its cursor starts at `delivery_start` with that prefix hash.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitSource {
    pub source_id: SourceId,
    pub delivery_start: u64,
    pub prefix_hash: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Initialized {
    pub channel: u64,
    pub sources: Vec<InitSource>,
    pub initial_anchor: u64,
    pub build_digest: String,
    pub at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OEra {
    pub switch_at: DateTime<Utc>,
    pub initial_channels: Vec<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HaltReason {
    StoreDamage,
    SpoolGap,
    SpoolTailMismatch,
}

/// The channel's O output stops and alarms from the first occurrence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Halt {
    pub reason: HaltReason,
    pub detail: String,
}

#[derive(Debug)]
pub enum StoreError {
    Io(io::Error),
    Halt(Halt),
    /// The channel spool is at its cap; the caller pauses the source and alarms.
    SpoolFull,
    /// An API precondition failed; nothing was written.
    Rejected(String),
}

impl StoreError {
    pub(super) fn halt(reason: HaltReason, detail: String) -> Self {
        Self::Halt(Halt { reason, detail })
    }
}

impl From<io::Error> for StoreError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(error: serde_json::Error) -> Self {
        Self::Rejected(format!("encode: {error}"))
    }
}

impl From<StoreError> for Halt {
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::Halt(halt) => halt,
            other => Self {
                reason: HaltReason::StoreDamage,
                detail: format!("{other:?}"),
            },
        }
    }
}

pub(super) fn damage(detail: impl Into<String>) -> StoreError {
    StoreError::halt(HaltReason::StoreDamage, detail.into())
}

/// `<runtime_root>/o_store`; the enabled flag is the only way to obtain one.
pub struct OStore {
    root: PathBuf,
}

impl OStore {
    pub fn open_if_enabled(config: &StoreConfig, runtime_root: &Path) -> io::Result<Option<Self>> {
        Self::open_checked(config, runtime_root, PARENT_DIR_FSYNC_FLUSHES)
    }

    /// Refuses to enable where a directory fsync is a no-op: names and cursors would not be durable.
    fn open_checked(
        config: &StoreConfig,
        runtime_root: &Path,
        dir_fsync_flushes: bool,
    ) -> io::Result<Option<Self>> {
        if !config.enabled {
            return Ok(None);
        }
        if !dir_fsync_flushes {
            let detail = "o_store needs a directory fsync this platform does not provide";
            return Err(io::Error::new(io::ErrorKind::Unsupported, detail));
        }
        let root = runtime_root.join(STORE_DIR_NAME);
        durable::ensure_dir(&root)?;
        durable::sweep_tmp(&root)?;
        Ok(Some(Self { root }))
    }

    /// The store as an operator tool reads it: nothing is created or swept.
    pub fn existing(runtime_root: &Path) -> Option<Self> {
        let root = runtime_root.join(STORE_DIR_NAME);
        root.is_dir().then_some(Self { root })
    }

    fn channel_dir(&self, channel: u64) -> PathBuf {
        self.root.join(channel.to_string())
    }

    /// Whether anything of the channel's store may exist, even without its `init`; an unreadable
    /// entry counts.
    pub fn has_channel_dir(&self, channel: u64) -> bool {
        let entry = std::fs::symlink_metadata(self.channel_dir(channel));
        !matches!(entry, Err(error) if error.kind() == io::ErrorKind::NotFound)
    }

    /// Channels whose directory holds an `init` entry, readable or not. Only canonical ids count,
    /// so `042` never stands for channel 42.
    pub fn channels_with_init(&self) -> io::Result<std::collections::BTreeSet<u64>> {
        let mut channels = std::collections::BTreeSet::new();
        for entry in std::fs::read_dir(&self.root)? {
            let name = entry?.file_name();
            let channel = name.to_str().and_then(|name| {
                let channel = name.parse::<u64>().ok()?;
                (channel != 0 && channel.to_string() == name).then_some(channel)
            });
            let Some(channel) = channel else { continue };
            let init = std::fs::symlink_metadata(self.channel_dir(channel).join(INIT_FILE));
            if !matches!(init, Err(error) if error.kind() == io::ErrorKind::NotFound) {
                channels.insert(channel);
            }
        }
        Ok(channels)
    }

    pub fn read_era(&self) -> Result<Option<OEra>, StoreError> {
        durable::read_json(&self.root.join(ERA_FILE))
    }

    pub fn read_init(&self, channel: u64) -> Result<Option<Initialized>, StoreError> {
        let init: Option<Initialized> =
            durable::read_json(&self.channel_dir(channel).join(INIT_FILE))?;
        match init {
            Some(init) if init.channel != channel => Err(damage("init names another channel")),
            init => Ok(init),
        }
    }

    /// Lays out the channel and an empty ledger, then publishes `init` exactly once.
    /// An era channel is never initialized again, even when its `init` is gone.
    pub fn init_channel(&self, init: &Initialized) -> Result<(), StoreError> {
        let sealed = self.read_era()?.map(|era| era.initial_channels);
        if sealed.is_some_and(|channels| channels.contains(&init.channel)) {
            return Err(damage("era channel cannot be initialized again"));
        }
        let dir = self.channel_dir(init.channel);
        durable::ensure_dir(&dir)?;
        durable::ensure_dir(&dir.join(SPOOL_DIR))?;
        durable::ensure_dir(&dir.join(CURSOR_DIR))?;
        ledger::create_empty(&dir.join(LEDGER_FILE))?;
        Ok(durable::create_once(
            &dir.join(INIT_FILE),
            &serde_json::to_vec(init)?,
        )?)
    }

    /// First writer start: initializes only channels still lacking `init`, then seals `o_era` once.
    /// A sealed era is returned unchanged; a damaged pre-era `init` stops the switch.
    pub fn begin_era(
        &self,
        channels: &[u64],
        switch_at: DateTime<Utc>,
        mut init_for: impl FnMut(u64) -> Result<Initialized, StoreError>,
    ) -> Result<OEra, StoreError> {
        if let Some(era) = self.read_era()? {
            return Ok(era);
        }
        for &channel in channels {
            if self.read_init(channel)?.is_none() {
                let init = init_for(channel)?;
                if init.channel != channel {
                    return Err(StoreError::Rejected("init for another channel".into()));
                }
                self.init_channel(&init)?;
            }
        }
        let initial_channels = channels.to_vec();
        let era = OEra {
            switch_at,
            initial_channels,
        };
        durable::create_once(&self.root.join(ERA_FILE), &serde_json::to_vec(&era)?)?;
        Ok(era)
    }

    /// Recovers one channel; `None` means it has no store and is not an era channel.
    pub fn open_channel(&self, era: &OEra, channel: u64) -> Result<Option<ChannelStore>, Halt> {
        let init = match self.read_init(channel)? {
            Some(init) => init,
            None if era.initial_channels.contains(&channel) => {
                return Err(damage("era channel has no init").into());
            }
            None => return Ok(None),
        };
        let dir = self.channel_dir(channel);
        for swept in [dir.clone(), dir.join(SPOOL_DIR), dir.join(CURSOR_DIR)] {
            durable::sweep_tmp(&swept).map_err(StoreError::from)?;
        }
        let ledger = ledger::recover(&dir.join(LEDGER_FILE), init.initial_anchor)?;
        let sources = spool::recover_sources(&dir, &init, &ledger)?;
        let (segment_max, spool_cap) = (spool::SEGMENT_MAX_BYTES, spool::SPOOL_CAP_BYTES);
        Ok(Some(ChannelStore {
            dir,
            init,
            ledger,
            sources,
            segment_max,
            spool_cap,
            failed: false,
        }))
    }
}

/// One recovered channel. After an I/O error it refuses writes until reopened through recovery.
pub struct ChannelStore {
    dir: PathBuf,
    init: Initialized,
    ledger: LedgerState,
    sources: BTreeMap<String, spool::SourceSpool>,
    segment_max: u64,
    spool_cap: u64,
    failed: bool,
}

impl ChannelStore {
    pub fn init(&self) -> &Initialized {
        &self.init
    }

    pub fn ledger(&self) -> &LedgerState {
        &self.ledger
    }

    /// Delivery entries only; `SpoolGc` is written solely by the GC path that deletes the segment.
    pub fn append_ledger(&mut self, entry: LedgerEntry) -> Result<(), StoreError> {
        if matches!(entry, LedgerEntry::SpoolGc { .. }) {
            return Err(StoreError::Rejected("SpoolGc is written by GC only".into()));
        }
        self.mutate(|store| store.write_ledger(entry))
    }

    fn write_ledger(&mut self, entry: LedgerEntry) -> Result<(), StoreError> {
        let at = Utc::now();
        ledger::append(&self.dir.join(LEDGER_FILE), at, &entry)?;
        self.ledger.apply(at, entry);
        Ok(())
    }

    fn mutate<T>(
        &mut self,
        op: impl FnOnce(&mut Self) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        if self.failed {
            return Err(StoreError::Rejected(
                "reopen the channel after an I/O error".into(),
            ));
        }
        let result = op(self);
        self.failed = matches!(result, Err(StoreError::Io(_)));
        result
    }
}

#[cfg(test)]
#[cfg(windows)]
mod windows_tests {
    use super::*;

    #[test]
    fn enabling_the_store_is_refused_without_directory_fsync() {
        let runtime = tempfile::tempdir().unwrap();
        let config = StoreConfig { enabled: true };
        assert!(!PARENT_DIR_FSYNC_FLUSHES);
        assert_eq!(
            OStore::open_checked(&config, runtime.path(), PARENT_DIR_FSYNC_FLUSHES)
                .err()
                .map(|error| error.kind()),
            Some(io::ErrorKind::Unsupported)
        );
        assert!(!runtime.path().join(STORE_DIR_NAME).exists());
        assert!(
            OStore::open_if_enabled(&StoreConfig { enabled: false }, runtime.path())
                .unwrap()
                .is_none()
        );
    }
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    use crate::services::tui_o::shadow::{ShadowProvider, UnitKey, UnitKind};

    pub(super) fn enabled(runtime: &Path) -> OStore {
        let config = StoreConfig { enabled: true };
        OStore::open_if_enabled(&config, runtime).unwrap().unwrap()
    }

    pub(super) fn initialized(channel: u64, sources: Vec<InitSource>) -> Initialized {
        let (initial_anchor, build_digest, at) = (100, "build-a".to_string(), Utc::now());
        Initialized {
            channel,
            sources,
            initial_anchor,
            build_digest,
            at,
        }
    }

    pub(super) fn sealed(store: &OStore, channels: &[u64]) -> OEra {
        store
            .begin_era(channels, Utc::now(), |channel| {
                Ok(initialized(channel, Vec::new()))
            })
            .unwrap()
    }

    fn unit(native_key: &str) -> UnitKey {
        let (provider, kind) = (ShadowProvider::Claude, UnitKind::Body);
        UnitKey {
            channel_id: 7,
            provider,
            native_key: native_key.into(),
            kind,
        }
    }

    fn prepared(serial: u64, anchor_id: u64) -> LedgerEntry {
        let (unit_key, payload) = (unit(&format!("m{serial}")), format!("piece {serial}"));
        LedgerEntry::Prepared {
            serial,
            unit_key,
            piece_index: 0,
            payload,
            anchor_id,
            epoch: 3,
        }
    }

    fn halt_reason(result: Result<Option<ChannelStore>, Halt>) -> HaltReason {
        match result {
            Err(halt) => halt.reason,
            Ok(store) => panic!("expected a halt, opened: {}", store.is_some()),
        }
    }

    #[test]
    fn the_store_is_dormant_unless_its_flag_is_set() {
        let runtime = tempfile::tempdir().unwrap();
        let config: StoreConfig = serde_json::from_str("{}").unwrap();
        assert!(!config.enabled);
        assert!(
            OStore::open_if_enabled(&config, runtime.path())
                .unwrap()
                .is_none()
        );
        assert!(!runtime.path().join(STORE_DIR_NAME).exists());
    }

    #[test]
    fn enabling_the_store_is_refused_where_directory_fsync_is_a_no_op() {
        let runtime = tempfile::tempdir().unwrap();
        let config = StoreConfig { enabled: true };
        let refused = OStore::open_checked(&config, runtime.path(), false);
        assert_eq!(
            refused.err().map(|error| error.kind()),
            Some(io::ErrorKind::Unsupported)
        );
        assert!(!runtime.path().join(STORE_DIR_NAME).exists());
    }

    #[test]
    fn init_and_ledger_round_trip_the_anchor_serials_and_outcomes() {
        let runtime = tempfile::tempdir().unwrap();
        let store = enabled(runtime.path());
        let era = sealed(&store, &[7]);
        let mut channel = store.open_channel(&era, 7).unwrap().unwrap();
        for entry in [
            prepared(0, 100),
            LedgerEntry::Posted {
                serial: 0,
                msg_id: 200,
            },
            prepared(1, 200),
            LedgerEntry::Excluded {
                unit_key: unit("tool"),
                reason: "normal_tool_result".into(),
            },
        ] {
            channel.append_ledger(entry).unwrap();
        }
        let reopened = store.open_channel(&era, 7).unwrap().unwrap();
        assert_eq!(reopened.init(), channel.init());
        let ledger = reopened.ledger();
        assert_eq!((ledger.anchor(), ledger.next_serial()), (200, 2));
        assert_eq!(
            ledger
                .unresolved()
                .map(|(serial, piece)| (serial, piece.epoch)),
            Some((1, 3))
        );
        let posted = ledger
            .latest_piece(&unit("m0"), 0)
            .and_then(|(_, piece)| piece.outcome.clone());
        assert_eq!(posted, Some(ledger::PieceOutcome::Posted(200)));
        assert_eq!(ledger.excluded(&unit("tool")), Some("normal_tool_result"));
        assert_eq!(ledger.violation(), None);
        assert_eq!(ledger, channel.ledger());
    }

    #[test]
    fn a_crash_before_the_era_initializes_only_channels_without_init() {
        let runtime = tempfile::tempdir().unwrap();
        let store = enabled(runtime.path());
        let survivor = initialized(1, Vec::new());
        store.init_channel(&survivor).unwrap();
        let mut asked = Vec::new();
        let era = store
            .begin_era(&[1, 2], Utc::now(), |channel| {
                asked.push(channel);
                Ok(initialized(channel, Vec::new()))
            })
            .unwrap();
        assert_eq!((asked, era.initial_channels.clone()), (vec![2], vec![1, 2]));
        assert_eq!(store.read_init(1).unwrap(), Some(survivor.clone()));
        assert!(store.init_channel(&initialized(1, Vec::new())).is_err());
        let again = store.begin_era(&[1, 2, 3], Utc::now(), |_| panic!("era already sealed"));
        assert_eq!(again.unwrap(), era);
        assert_eq!(store.read_init(1).unwrap(), Some(survivor));
    }

    #[test]
    fn missing_or_damaged_state_of_an_era_channel_halts_instead_of_reinitializing() {
        let runtime = tempfile::tempdir().unwrap();
        let store = enabled(runtime.path());
        let era = sealed(&store, &[1, 2, 3]);
        let dir = |channel: u64| {
            runtime
                .path()
                .join(STORE_DIR_NAME)
                .join(channel.to_string())
        };
        std::fs::remove_file(dir(1).join(INIT_FILE)).unwrap();
        std::fs::write(dir(2).join(INIT_FILE), b"{\"channel\":").unwrap();
        let mut third = store.open_channel(&era, 3).unwrap().unwrap();
        third.append_ledger(prepared(0, 100)).unwrap();
        third
            .append_ledger(LedgerEntry::NotFound { serial: 0 })
            .unwrap();
        let ledger = dir(3).join(LEDGER_FILE);
        let text = std::fs::read_to_string(&ledger)
            .unwrap()
            .replacen("\"at\"", "\"a!\"", 1);
        std::fs::write(&ledger, text).unwrap();
        for channel in [1, 2, 3] {
            assert_eq!(
                halt_reason(store.open_channel(&era, channel)),
                HaltReason::StoreDamage
            );
        }
        assert_eq!(
            store
                .begin_era(&[1, 2, 3], Utc::now(), |_| panic!("re-init"))
                .unwrap(),
            era
        );
        assert!(store.init_channel(&initialized(1, Vec::new())).is_err());
        assert!(!dir(1).join(INIT_FILE).exists());
        assert!(store.open_channel(&era, 9).unwrap().is_none());
    }

    #[test]
    fn a_torn_ledger_append_is_cut_so_the_channel_reopens_and_keeps_appending() {
        let runtime = tempfile::tempdir().unwrap();
        let store = enabled(runtime.path());
        let era = sealed(&store, &[7]);
        let mut channel = store.open_channel(&era, 7).unwrap().unwrap();
        channel.append_ledger(prepared(0, 100)).unwrap();
        let path = runtime
            .path()
            .join(STORE_DIR_NAME)
            .join("7")
            .join(LEDGER_FILE);
        durable::append_synced(&path, br#"{"at":"2026-09-29T00:00:00Z","entry":{"ty"#).unwrap();
        let mut reopened = store.open_channel(&era, 7).unwrap().unwrap();
        assert_eq!(reopened.ledger(), channel.ledger());
        reopened
            .append_ledger(LedgerEntry::Posted {
                serial: 0,
                msg_id: 300,
            })
            .unwrap();
        let last = store.open_channel(&era, 7).unwrap().unwrap();
        assert_eq!(
            (last.ledger().anchor(), last.ledger().unresolved()),
            (300, None)
        );
    }
}

#[cfg(test)]
impl ChannelStore {
    /// Shrinks the segment and spool limits so a test can fill the spool.
    pub(crate) fn set_limits_for_test(&mut self, segment_max: u64, spool_cap: u64) {
        (self.segment_max, self.spool_cap) = (segment_max, spool_cap);
    }
}
