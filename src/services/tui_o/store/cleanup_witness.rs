//! Non-authoritative, cursor-bound observations used only for missing-source cleanup.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::spool::{Cursor, source_key};
use super::{ChannelStore, StoreError, damage, durable};
use crate::services::tui_o::shadow::SourceId;
use crate::services::tui_o::shadow::capture::Fingerprint;

pub const CLEANUP_DIR: &str = "cleanup_witness";
pub const SAFE_AGE: Duration = Duration::from_secs(24 * 3600);
pub const MAX_MISSING_GAP: Duration = Duration::from_secs(23 * 3600);
pub const WITNESS_REFRESH: Duration = Duration::from_secs(3600);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupWitness {
    pub source: SourceId,
    pub captured_through: u64,
    pub prefix_hash: String,
    pub fingerprint: Fingerprint,
    pub observed_at: SystemTime,
}

impl CleanupWitness {
    fn validate(&self, source: &SourceId) -> Result<(), StoreError> {
        let hash = self.prefix_hash.as_bytes();
        if self.source != *source
            || self.fingerprint.ino != source.ino
            || self.fingerprint.len != self.captured_through
            || !(0..1_000_000_000).contains(&self.fingerprint.change.1)
            || hash.len() != 64
            || !hash.iter().all(u8::is_ascii_hexdigit)
            || self.observed_at.duration_since(UNIX_EPOCH).is_err()
            || self
                .fingerprint
                .modified
                .duration_since(UNIX_EPOCH)
                .is_err()
        {
            return Err(damage("malformed cleanup witness"));
        }
        Ok(())
    }

    pub fn bound_to(&self, cursor: &Cursor) -> bool {
        !cursor.retired
            && self.source == cursor.source
            && self.captured_through == cursor.captured_through
            && self.prefix_hash == cursor.prefix_hash
    }

    pub fn eligible(&self, now: SystemTime) -> bool {
        self.observed_at
            .duration_since(self.fingerprint.modified)
            .is_ok()
            && now
                .duration_since(self.fingerprint.modified)
                .is_ok_and(|age| age >= SAFE_AGE)
            && now
                .duration_since(self.observed_at)
                .is_ok_and(|gap| gap <= MAX_MISSING_GAP)
    }
}

impl ChannelStore {
    /// Called only by an explicitly selected cleanup engine, including old stores without this dir.
    pub fn prepare_cleanup_witnesses(&mut self) -> Result<(), StoreError> {
        self.mutate(|store| {
            let dir = store.dir.join(CLEANUP_DIR);
            durable::ensure_dir(&dir)?;
            Ok(durable::sweep_tmp(&dir)?)
        })
    }

    pub fn cleanup_witness(&self, source: &SourceId) -> Result<Option<CleanupWitness>, StoreError> {
        let path = self
            .dir
            .join(CLEANUP_DIR)
            .join(format!("{}.json", source_key(source)));
        let witness: Option<CleanupWitness> = durable::read_json(&path)?;
        let Some(witness) = witness else {
            return Ok(None);
        };
        witness.validate(source)?;
        let cursor = self
            .cursor(source)
            .ok_or_else(|| damage("witness source not attached"))?;
        Ok(witness.bound_to(cursor).then_some(witness))
    }

    /// The stored witness once it validates, whether or not the cursor is retired. A read of the
    /// record only: unlike `cleanup_witness` it grants no missing-source cleanup.
    pub(crate) fn recorded_witness(
        &self,
        source: &SourceId,
    ) -> Result<Option<CleanupWitness>, StoreError> {
        let name = format!("{}.json", source_key(source));
        let witness: Option<CleanupWitness> =
            durable::read_json(&self.dir.join(CLEANUP_DIR).join(name))?;
        witness.map(|w| w.validate(source).map(|()| w)).transpose()
    }

    pub fn write_cleanup_witness(&mut self, witness: &CleanupWitness) -> Result<(), StoreError> {
        self.mutate(|store| {
            witness.validate(&witness.source)?;
            let cursor = store
                .cursor(&witness.source)
                .ok_or_else(|| damage("witness source not attached"))?;
            if !witness.bound_to(cursor) {
                return Err(StoreError::Rejected(
                    "cleanup witness cursor changed".into(),
                ));
            }
            let path = store
                .dir
                .join(CLEANUP_DIR)
                .join(format!("{}.json", source_key(&witness.source)));
            Ok(durable::replace(&path, &serde_json::to_vec(witness)?)?)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::tui_o::shadow::binding_reader::source_id_for;
    use crate::services::tui_o::store::durable::fault::{self, Step};
    use crate::services::tui_o::store::{InitSource, Initialized, OStore, StoreConfig};
    use sha2::{Digest, Sha256};

    fn fixture() -> (tempfile::TempDir, OStore, ChannelStore, CleanupWitness) {
        let runtime = tempfile::tempdir().unwrap();
        let path = runtime.path().join("source.jsonl");
        std::fs::write(&path, b"").unwrap();
        let source = source_id_for("session", &path).unwrap();
        let prefix_hash = hex::encode(Sha256::digest(b""));
        let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, runtime.path())
            .unwrap()
            .unwrap();
        let now = chrono::Utc::now();
        let init = Initialized {
            channel: 7,
            at: now,
            initial_anchor: 0,
            sources: vec![InitSource {
                source_id: source.clone(),
                delivery_start: 0,
                prefix_hash: prefix_hash.clone(),
            }],
            build_digest: "test".into(),
        };
        let era = store.begin_era(&[7], now, |_| Ok(init.clone())).unwrap();
        let channel = store.open_channel(&era, 7).unwrap().unwrap();
        let witness = CleanupWitness {
            source: source.clone(),
            captured_through: 0,
            prefix_hash,
            fingerprint: Fingerprint::path(&source).unwrap(),
            observed_at: SystemTime::now(),
        };
        (runtime, store, channel, witness)
    }

    #[test]
    fn readers_s1a_witness_init_only_and_old_directory_recover() {
        let (runtime, store, mut channel, witness) = fixture();
        let dir = runtime.path().join("o_store/7");
        assert!(!dir.join(CLEANUP_DIR).exists());
        assert!(
            !dir.join("cursor")
                .join(format!("{}.json", source_key(&witness.source)))
                .exists()
        );
        channel.prepare_cleanup_witnesses().unwrap();
        channel.write_cleanup_witness(&witness).unwrap();
        std::fs::write(dir.join(CLEANUP_DIR).join("crash.tmp"), b"torn").unwrap();
        drop(channel);
        let era = store.read_era().unwrap().unwrap();
        let mut reopened = store.open_channel(&era, 7).unwrap().unwrap();
        reopened.prepare_cleanup_witnesses().unwrap();
        assert_eq!(
            reopened.cleanup_witness(&witness.source).unwrap(),
            Some(witness)
        );
        assert!(!dir.join(CLEANUP_DIR).join("crash.tmp").exists());
    }

    #[test]
    fn readers_s1a_witness_write_failure_latches_and_keeps_prior_copy() {
        let (runtime, _, mut channel, mut witness) = fixture();
        let dir = runtime.path().join("o_store/7").join(CLEANUP_DIR);
        channel.prepare_cleanup_witnesses().unwrap();
        channel.write_cleanup_witness(&witness).unwrap();
        let old = channel.cleanup_witness(&witness.source).unwrap();
        let planted = fault::plant(&dir, Step::Write, std::io::ErrorKind::StorageFull, Some(1));
        witness.observed_at += Duration::from_secs(1);
        assert!(channel.write_cleanup_witness(&witness).is_err());
        drop(planted);
        assert_eq!(channel.io_failure(), Some(std::io::ErrorKind::StorageFull));
        assert!(channel.set_retired(&witness.source, true).is_err());
        assert_eq!(channel.cleanup_witness(&witness.source).unwrap(), old);
    }

    #[test]
    fn readers_s1a_malformed_witness_is_not_cleanup_authority() {
        let (runtime, _, mut channel, witness) = fixture();
        channel.prepare_cleanup_witnesses().unwrap();
        let path = runtime
            .path()
            .join("o_store/7")
            .join(CLEANUP_DIR)
            .join(format!("{}.json", source_key(&witness.source)));
        std::fs::write(&path, b"{torn").unwrap();
        assert!(channel.cleanup_witness(&witness.source).is_err());
        assert!(!channel.cursor(&witness.source).unwrap().retired);
        let mut invalid = witness.clone();
        invalid.captured_through = 1;
        assert!(channel.write_cleanup_witness(&invalid).is_err());
        let now = SystemTime::now();
        let mut aged = witness;
        aged.fingerprint.modified = now - SAFE_AGE;
        aged.observed_at = now - MAX_MISSING_GAP;
        assert!(aged.eligible(now));
        assert!(!aged.eligible(now + Duration::from_secs(1)));
        aged.observed_at = now + Duration::from_secs(1);
        assert!(!aged.eligible(now));
    }

    #[test]
    fn readers_s1a_stale_offset_or_hash_witness_is_ignored() {
        let (runtime, _, mut channel, witness) = fixture();
        channel.prepare_cleanup_witnesses().unwrap();
        let path = runtime
            .path()
            .join("o_store/7")
            .join(CLEANUP_DIR)
            .join(format!("{}.json", source_key(&witness.source)));
        for change_offset in [false, true] {
            let mut stale = witness.clone();
            if change_offset {
                stale.captured_through = 1;
                stale.fingerprint.len = 1;
            } else {
                stale.prefix_hash = "0".repeat(64);
            }
            std::fs::write(&path, serde_json::to_vec(&stale).unwrap()).unwrap();
            assert_eq!(channel.cleanup_witness(&witness.source).unwrap(), None);
            assert!(!channel.cursor(&witness.source).unwrap().retired);
        }
    }
}
