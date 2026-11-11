use super::*;
use crate::services::discord::runtime_store;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Fault {
    None,
    Before,
    #[cfg(test)]
    AfterRename,
    Third,
    Read,
    Sync,
    BeforeProjection,
}

impl Owner {
    pub(super) fn commit(&mut self, next: SnapshotV2) -> Result<(), String> {
        if self.pending.is_some() {
            return Err("commit unknown: recovery required".into());
        }
        let candidate = serde_json::to_string_pretty(&next).map_err(|e| e.to_string())?;
        let actual = read(&self.path)?;
        if self.last_success_sequence == 0 {
            self.observe_snapshot(actual.as_deref(), &candidate, false);
        }
        if actual != self.bytes {
            self.observe_snapshot(actual.as_deref(), &candidate, true);
            return Err("snapshot changed outside owner".into());
        }
        if Some(&candidate) == self.bytes.as_ref() {
            self.snapshot = next;
            self.last_success_sequence = self.last_success_sequence.saturating_add(1);
            return Ok(());
        }
        let mut sync_unconfirmed = false;
        let result = if self.fault == Fault::Before {
            Err("before publish".into())
        } else {
            runtime_store::durable_publish::critical_publish_status(
                &self.path,
                &candidate,
                runtime_store::AtomicWriteContext::new("queued_input_v2"),
            )
            .and_then(|status| {
                if matches!(
                    status,
                    runtime_store::durable_publish::FlushStatus::SyncFailed { .. }
                ) {
                    sync_unconfirmed = true;
                    return Err("directory sync unconfirmed".into());
                }
                if self.fault != Fault::None {
                    Err("after publish".into())
                } else {
                    Ok(())
                }
            })
        };
        if result.is_err() {
            if self.fault == Fault::Third {
                std::fs::write(&self.path, b"third state").map_err(|e| e.to_string())?;
            }
            self.pending = Some((candidate, next));
            if sync_unconfirmed {
                return Err("directory sync unconfirmed; recover before proceeding".into());
            }
            return self.recover();
        }
        self.bytes = Some(candidate);
        self.snapshot = next;
        self.last_success_sequence = self.last_success_sequence.saturating_add(1);
        Ok(())
    }

    pub(crate) fn recover(&mut self) -> Result<(), String> {
        let Some((candidate, _)) = self.pending.as_ref() else {
            return Ok(());
        };
        let actual = if self.fault == Fault::Read {
            return Err("commit read unavailable".into());
        } else {
            read(&self.path)?
        };
        if actual.as_ref() == Some(candidate) {
            if self.fault == Fault::Sync {
                return Err("candidate sync unavailable".into());
            }
            // Flush a writable file handle; parent directories use the shared sync helper.
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&self.path)
                .and_then(|file| file.sync_all())
                .map_err(|e| e.to_string())?;
            let status = runtime_store::durable_publish::retry_published_status(
                &self.path,
                runtime_store::AtomicWriteContext::new("queued_input_v2"),
            );
            if matches!(
                status,
                runtime_store::durable_publish::FlushStatus::SyncFailed { .. }
            ) {
                return Err("candidate directory sync unavailable".into());
            }
            let (bytes, snapshot) = self.pending.take().ok_or("candidate disappeared")?;
            self.bytes = Some(bytes);
            self.snapshot = snapshot;
            self.last_success_sequence = self.last_success_sequence.saturating_add(1);
            self.project_slots();
            Ok(())
        } else if actual == self.bytes {
            self.pending = None;
            Err("old snapshot confirmed; transition not committed".into())
        } else {
            Err("commit unknown: preserve original and candidate; inspect storage manually".into())
        }
    }

    /// Correlates adoptions and conflicts without exposing queue contents or namespace/path text.
    fn observe_snapshot(&self, actual: Option<&str>, candidate: &str, mismatch: bool) {
        let expected = self.bytes.as_deref();
        let observation = if mismatch { "cas_mismatch" } else { "adopt" };
        macro_rules! record {
            ($level:path) => {
                tracing::event!(
                    $level,
                    observation,
                    namespace_sha256 = %fingerprint(self.snapshot.namespace.as_deref().map(str::as_bytes)),
                    path_sha256 = %fingerprint(Some(self.path.as_os_str().as_encoded_bytes())),
                    owner_pid = std::process::id(),
                    owner_incarnation = %self.incarnation,
                    last_success_sequence = self.last_success_sequence,
                    last_admission_sequence = self.snapshot.admission_sequence,
                    expected_sha256 = %fingerprint(expected.map(str::as_bytes)),
                    expected_bytes = expected.map_or(0, str::len),
                    expected_present = expected.is_some(),
                    actual_sha256 = %fingerprint(actual.map(str::as_bytes)),
                    actual_bytes = actual.map_or(0, str::len),
                    actual_present = actual.is_some(),
                    candidate_sha256 = %fingerprint(Some(candidate.as_bytes())),
                    candidate_bytes = candidate.len(),
                    "queued input owner snapshot observation"
                );
            };
        }
        if mismatch {
            record!(tracing::Level::WARN);
        } else {
            record!(tracing::Level::INFO);
        }
    }
}

fn fingerprint(bytes: Option<&[u8]>) -> String {
    use sha2::{Digest, Sha256};
    bytes.map_or_else(
        || "absent".into(),
        |bytes| hex::encode(Sha256::digest(bytes)),
    )
}

fn read(path: &std::path::Path) -> Result<Option<String>, String> {
    match std::fs::read_to_string(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}
