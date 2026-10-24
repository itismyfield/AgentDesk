//! Only an OS adapter can create the identity of an executable already mapped by a process.

use super::super::Unverified;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LoadedImage {
    dev: u64,
    ino: u64,
}

impl LoadedImage {
    pub(super) fn matches(&self, checker: &super::FileToken) -> bool {
        (self.dev, self.ino) == checker.dev_ino()
    }
}

#[cfg(target_os = "linux")]
pub(super) fn for_process(pid: u32) -> Result<LoadedImage, Unverified> {
    use std::os::unix::fs::MetadataExt;
    // The procfs magic link names the mapped executable even after its pathname is replaced.
    let mapped = std::fs::metadata(format!("/proc/{pid}/exe")).map_err(|_| Unverified::Identity)?;
    Ok(LoadedImage {
        dev: mapped.dev(),
        ino: mapped.ino(),
    })
}

#[cfg(not(target_os = "linux"))]
pub(super) fn for_process(_pid: u32) -> Result<LoadedImage, Unverified> {
    // No safe vnode-identity adapter is available here; a pathname is not loaded-image evidence.
    Err(Unverified::Identity)
}

#[cfg(test)]
pub(super) fn fixture(identity: (u64, u64)) -> LoadedImage {
    LoadedImage {
        dev: identity.0,
        ino: identity.1,
    }
}
