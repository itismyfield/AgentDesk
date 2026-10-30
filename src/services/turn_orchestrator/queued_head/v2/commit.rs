use super::*;
use crate::services::discord::runtime_store;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Fault {
    None,
    Before,
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
        // A grant names one queue front; a commit that removes or replaces queued items revokes it.
        let (was, now) = (&self.snapshot.items, &next.items);
        if now.len() < was.len()
            || was
                .first()
                .zip(now.first())
                .is_some_and(|(a, b)| !same(a, b))
        {
            self.front_grant = None;
        }
        let candidate = serde_json::to_string_pretty(&next).map_err(|e| e.to_string())?;
        let actual = read(&self.path)?;
        if actual != self.bytes {
            return Err("snapshot changed outside owner".into());
        }
        if Some(&candidate) == self.bytes.as_ref() {
            self.snapshot = next;
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
                if status != runtime_store::durable_publish::FlushStatus::Flushed {
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
            if status != runtime_store::durable_publish::FlushStatus::Flushed {
                return Err("candidate directory sync unavailable".into());
            }
            let (bytes, snapshot) = self.pending.take().ok_or("candidate disappeared")?;
            self.bytes = Some(bytes);
            self.snapshot = snapshot;
            self.project_slots();
            Ok(())
        } else if actual == self.bytes {
            self.pending = None;
            Err("old snapshot confirmed; transition not committed".into())
        } else {
            Err("commit unknown: preserve original and candidate; inspect storage manually".into())
        }
    }
}

fn read(path: &std::path::Path) -> Result<Option<String>, String> {
    match std::fs::read_to_string(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}
