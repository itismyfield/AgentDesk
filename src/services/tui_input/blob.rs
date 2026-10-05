use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::durable::{self, invalid};
use super::ledger::Ledger;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobPin {
    pub local_path: PathBuf,
    pub sha256: String,
    pub pinned: bool,
}

fn component(value: &str) -> io::Result<()> {
    let mut parts = Path::new(value).components();
    if !value.contains(['/', '\\'])
        && matches!(parts.next(), Some(Component::Normal(_)))
        && parts.next().is_none()
    {
        Ok(())
    } else {
        Err(invalid("invalid blob path component"))
    }
}

impl Ledger {
    // Pins have no age-based deletion path; the later row lifecycle owns release after termination.
    pub fn pin_blob(
        &self,
        row_id: &str,
        index: u32,
        filename: &str,
        bytes: &[u8],
    ) -> io::Result<BlobPin> {
        component(row_id)?;
        component(filename)?;
        let relative = Path::new("blobs")
            .join("att")
            .join(row_id)
            .join(format!("{index}_{filename}"));
        let path = self.dir.join(&relative);
        let parent = path
            .parent()
            .ok_or_else(|| invalid("missing blob parent"))?;
        durable::ensure_dir(parent)?;
        let pin = BlobPin {
            local_path: relative,
            sha256: hex::encode(Sha256::digest(bytes)),
            pinned: true,
        };
        let tmp = parent.join(format!(".blob-{}.tmp", uuid::Uuid::new_v4()));
        let mut file = durable::open_file(&tmp, true)?;
        let result = (|| -> io::Result<()> {
            #[cfg(test)]
            super::durability_tests::partial_blob_write(&mut file, bytes)?;
            file.write_all(bytes)?;
            durable::step("write", &tmp)?;
            durable::sync_file(&file, &tmp)?;
            match fs::hard_link(&tmp, &path) {
                Ok(()) => durable::step("link", &path)?,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    self.read_blob(&pin)?;
                    let file = durable::open_file(&path, false)?;
                    durable::sync_file(&file, &path)?;
                }
                Err(error) => return Err(error),
            }
            Ok(())
        })();
        // Own staging files are removed best-effort; pin retries ignore crash leftovers.
        // Leftovers are never adopted by pin_blob and remain for later garbage collection.
        let _ = fs::remove_file(&tmp);
        result?;
        durable::sync_dir(parent)?;
        Ok(pin)
    }

    pub fn read_blob(&self, pin: &BlobPin) -> io::Result<Vec<u8>> {
        read_pinned(&self.dir, pin)
    }
}

/// Verified pinned bytes read straight from a ledger directory, so no second handle is opened.
pub fn read_pinned(ledger_dir: &Path, pin: &BlobPin) -> io::Result<Vec<u8>> {
    let normalized: PathBuf = pin.local_path.components().collect();
    if !pin.pinned
        || !pin.local_path.starts_with("blobs/att")
        || pin.local_path.components().count() != 4
        || normalized.as_os_str() != pin.local_path.as_os_str()
    {
        return Err(invalid("invalid blob pin"));
    }
    let mut path = ledger_dir.to_path_buf();
    for part in pin.local_path.components() {
        let Component::Normal(part) = part else {
            return Err(invalid("invalid blob path"));
        };
        component(part.to_str().ok_or_else(|| invalid("invalid blob path"))?)?;
        path.push(part);
        if fs::symlink_metadata(&path)?.file_type().is_symlink() {
            return Err(invalid("symlink in blob path"));
        }
    }
    let bytes = fs::read(path)?;
    if hex::encode(Sha256::digest(&bytes)) != pin.sha256 {
        return Err(invalid("blob checksum mismatch"));
    }
    Ok(bytes)
}
