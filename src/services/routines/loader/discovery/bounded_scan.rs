use super::{
    DiscoveredRoutineScript, RoutineDiscoveryHooks, ValidatedRoutineRoot,
    collect_routine_scripts_inner, verify_discovery_authority,
};
#[cfg(unix)]
use super::{FileIdentity, UnixTraversalAuthority, open_root, require_available_identity};
#[cfg(unix)]
use std::collections::HashSet;
use std::io;
use std::path::Path;

const MAX_ROUTINE_TREE_ENTRIES: usize = 1_024;
const MAX_ROUTINE_TREE_FILES: usize = 256;
const MAX_ROUTINE_TREE_DEPTH: usize = 16;
const MAX_ROUTINE_TREE_SOURCE_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug)]
pub(super) struct RoutineTreeLimits {
    pub(super) max_entries: usize,
    pub(super) max_files: usize,
    pub(super) max_depth: usize,
    pub(super) max_source_bytes: u64,
}

pub(super) const DEFAULT_ROUTINE_TREE_LIMITS: RoutineTreeLimits = RoutineTreeLimits {
    max_entries: MAX_ROUTINE_TREE_ENTRIES,
    max_files: MAX_ROUTINE_TREE_FILES,
    max_depth: MAX_ROUTINE_TREE_DEPTH,
    max_source_bytes: MAX_ROUTINE_TREE_SOURCE_BYTES,
};

#[derive(Debug)]
pub(super) struct TraversalBudget {
    limits: RoutineTreeLimits,
    entries: usize,
    files: usize,
    source_bytes: u64,
}

impl TraversalBudget {
    pub(super) fn new(limits: RoutineTreeLimits) -> Self {
        Self {
            limits,
            entries: 0,
            files: 0,
            source_bytes: 0,
        }
    }

    pub(super) fn record_entry(&mut self, directory: &Path) -> io::Result<()> {
        self.entries = self.entries.checked_add(1).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "routine tree entry count overflow",
            )
        })?;
        if self.entries > self.limits.max_entries {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "routine tree under `{}` exceeds maximum entry count {}",
                    directory.display(),
                    self.limits.max_entries
                ),
            ));
        }
        Ok(())
    }

    pub(super) fn record_file(&mut self, path: &Path) -> io::Result<()> {
        self.files = self.files.checked_add(1).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "routine tree file count overflow",
            )
        })?;
        if self.files > self.limits.max_files {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "routine tree at `{}` exceeds maximum file count {}",
                    path.display(),
                    self.limits.max_files
                ),
            ));
        }
        Ok(())
    }

    pub(super) fn verify_depth(&self, depth: usize, path: &Path) -> io::Result<()> {
        if depth > self.limits.max_depth {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "routine tree at `{}` exceeds maximum depth {}",
                    path.display(),
                    self.limits.max_depth
                ),
            ));
        }
        Ok(())
    }

    pub(super) fn reserve_source(&mut self, bytes: u64, path: &Path) -> io::Result<()> {
        let total = self.source_bytes.checked_add(bytes).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "routine tree source byte count overflow",
            )
        })?;
        if total > self.limits.max_source_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "routine tree source bytes at `{}` exceed maximum {}",
                    path.display(),
                    self.limits.max_source_bytes
                ),
            ));
        }
        self.source_bytes = total;
        Ok(())
    }
}

pub(in super::super) fn collect_routine_script_paths(
    root: &ValidatedRoutineRoot,
    hooks: RoutineDiscoveryHooks<'_>,
) -> io::Result<Vec<DiscoveredRoutineScript>> {
    collect_routine_script_paths_with_limits(root, hooks, DEFAULT_ROUTINE_TREE_LIMITS)
}

pub(super) fn collect_routine_script_paths_with_limits(
    root: &ValidatedRoutineRoot,
    hooks: RoutineDiscoveryHooks<'_>,
    limits: RoutineTreeLimits,
) -> io::Result<Vec<DiscoveredRoutineScript>> {
    #[cfg(unix)]
    {
        verify_discovery_authority(hooks)?;
        let directory = open_root(root)?;
        let root_identity = require_available_identity(
            FileIdentity::from_metadata(&directory.metadata()?),
            &root.canonical,
        )?;
        if root.forbidden_entry_identities.contains(&root_identity) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "routine root `{}` aliases the protected routine helper surface",
                    root.canonical.display()
                ),
            ));
        }
        let mut authority = UnixTraversalAuthority {
            root_identity,
            root_mount_id: root.mount_id,
            forbidden_entry_identities: &root.forbidden_entry_identities,
            visited_directory_identities: HashSet::from([root_identity]),
        };
        let mut budget = TraversalBudget::new(limits);
        verify_discovery_authority(hooks)?;
        let mut out = Vec::new();
        collect_routine_scripts_inner(
            &directory,
            &root.canonical,
            0,
            hooks,
            &mut authority,
            &mut budget,
            &mut out,
        )?;
        verify_discovery_authority(hooks)?;
        Ok(out)
    }
    #[cfg(not(unix))]
    {
        verify_discovery_authority(hooks)?;
        let mut budget = TraversalBudget::new(limits);
        let mut out = Vec::new();
        collect_routine_scripts_inner(&root.canonical, 0, hooks, &mut budget, &mut out)?;
        verify_discovery_authority(hooks)?;
        Ok(out)
    }
}

pub(in super::super) fn require_nonempty_routine_tree(
    snapshots: &[DiscoveredRoutineScript],
) -> io::Result<()> {
    if snapshots.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "routine root contains no JavaScript entrypoints",
        ));
    }
    Ok(())
}
