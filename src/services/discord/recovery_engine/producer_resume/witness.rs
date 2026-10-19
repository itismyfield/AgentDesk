use sha2::{Digest, Sha256};
use std::fs::File;
use std::io;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Stamp(pub u64, pub u64, pub u64, pub i64, pub i64);
impl Stamp {
    pub(super) fn read(file: &File) -> io::Result<Self> {
        let m = file.metadata()?;
        if !m.is_file() {
            return Err(io::Error::other("source_not_regular"));
        }
        Ok(Self(m.dev(), m.ino(), m.len(), m.mtime(), m.mtime_nsec()))
    }
}
pub(super) struct Witness {
    path: PathBuf,
    stamp: Stamp,
    hash: Sha256,
    initial_len: u64,
    observed: std::cell::RefCell<Stamp>,
}
fn hash_prefix(file: &File, len: u64) -> io::Result<Sha256> {
    let mut hash = Sha256::new();
    let mut buf = [0; 16384];
    let mut at = 0;
    while at < len {
        let size = (len - at).min(buf.len() as u64) as usize;
        file.read_exact_at(&mut buf[..size], at)?;
        hash.update(&buf[..size]);
        at += size as u64;
    }
    Ok(hash)
}
impl Witness {
    pub(super) fn capture(path: &Path, file: &File) -> io::Result<Self> {
        let stamp = Stamp::read(file)?;
        let hash = hash_prefix(file, stamp.2)?;
        let result = Self {
            path: path.to_owned(),
            initial_len: stamp.2,
            observed: std::cell::RefCell::new(stamp.clone()),
            stamp,
            hash,
        };
        result.verify(file, false, true)?;
        Ok(result)
    }
    pub(super) fn verify(&self, file: &File, growth: bool, force: bool) -> io::Result<()> {
        let stamp = Stamp::read(file)?;
        let path = std::fs::metadata(&self.path)?;
        if (stamp.0, stamp.1) != (path.dev(), path.ino())
            || (stamp.0, stamp.1) != (self.stamp.0, self.stamp.1)
            || stamp.2 < self.observed.borrow().2
            || (!growth && stamp.2 != self.stamp.2)
        {
            return Err(io::Error::other("source_identity_or_size_changed"));
        }
        if (force
            || (growth && self.stamp.2 > self.initial_len)
            || stamp != *self.observed.borrow())
            && hash_prefix(file, self.stamp.2)?.finalize() != self.hash.clone().finalize()
        {
            return Err(io::Error::other("source_prefix_changed"));
        }
        *self.observed.borrow_mut() = stamp;
        Ok(())
    }
    pub(super) fn accepted(&mut self, at: u64, bytes: &[u8]) -> io::Result<()> {
        if at + bytes.len() as u64 <= self.stamp.2 {
            return Ok(());
        }
        if at != self.stamp.2 {
            return Err(io::Error::other("raw_record_gap"));
        }
        self.hash.update(bytes);
        self.stamp.2 += bytes.len() as u64;
        Ok(())
    }
    pub(super) fn appended(&mut self, file: &File, bytes: &[u8]) -> io::Result<()> {
        self.hash.update(bytes);
        let stamp = Stamp::read(file)?;
        if stamp.2 != self.stamp.2 + bytes.len() as u64 {
            return Err(io::Error::other("concurrent_wrapper_write"));
        }
        *self.observed.borrow_mut() = stamp.clone();
        self.stamp = stamp;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn pinned_prefix_identity_and_later_growth_cannot_be_rewritten_or_shrunk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("raw");
        std::fs::write(&path, b"old\n").unwrap();
        let file = File::open(&path).unwrap();
        let mut witness = Witness::capture(&path, &file).unwrap();
        let mut appender = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        appender.write_all(b"new\n").unwrap();
        witness.accepted(4, b"new\n").unwrap();
        witness.verify(&file, true, false).unwrap();
        std::fs::write(&path, b"old\nBAD\n").unwrap();
        assert!(
            witness.verify(&file, true, false).is_err(),
            "queued growth must retain its decoded bytes"
        );
        std::fs::write(&path, b"old\nnew\n").unwrap();
        witness.verify(&file, true, true).unwrap();
        appender.set_len(6).unwrap();
        assert!(
            witness.verify(&file, true, false).is_err(),
            "shrink after observed growth must hold"
        );
        let replacement = dir.path().join("replacement");
        std::fs::write(&replacement, b"old\nnew\n").unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        assert!(
            witness.verify(&file, true, false).is_err(),
            "path swaps must not reopen a different inode"
        );
    }
}
