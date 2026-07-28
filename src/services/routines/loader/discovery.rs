use super::{LoadedRoutineScript, RoutineScriptCandidate, full_source_version};
use anyhow::Result;
use std::collections::{BTreeMap, HashMap, HashSet};
#[cfg(unix)]
use std::ffi::{CStr, CString, OsStr, OsString};
use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

#[cfg(unix)]
use std::os::fd::{AsRawFd as _, FromRawFd as _};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt as _;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt as _;

const ROUTINE_HELPERS_DIR_NAME: &str = "routine-helpers";

#[derive(Debug)]
pub(super) enum PathResolutionError {
    Io {
        path: PathBuf,
        source: io::Error,
    },
    DanglingSymlink {
        path: PathBuf,
    },
    AmbiguousMissingPath {
        path: PathBuf,
    },
}

impl fmt::Display for PathResolutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(f, "failed to resolve `{}`: {source}", path.display())
            }
            Self::DanglingSymlink { path } => write!(
                f,
                "cannot resolve `{}` because its longest existing prefix is a dangling symlink",
                path.display()
            ),
            Self::AmbiguousMissingPath { path } => write!(
                f,
                "cannot safely resolve missing path `{}` because `..` follows a non-existent component",
                path.display()
            ),
        }
    }
}

impl std::error::Error for PathResolutionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::DanglingSymlink { .. } | Self::AmbiguousMissingPath { .. } => None,
        }
    }
}

#[derive(Debug)]
pub(super) enum RoutineRootValidationError {
    CurrentDirectoryUnavailable {
        source: io::Error,
    },
    RuntimeRootCanonicalization {
        runtime_root: PathBuf,
        source: PathResolutionError,
    },
    RuntimeRootAuthorityChanged {
        runtime_root: PathBuf,
        expected_canonical_root: PathBuf,
        observed_canonical_root: PathBuf,
    },
    RootCanonicalization {
        root_index: usize,
        root: PathBuf,
        source: PathResolutionError,
    },
    RootAuthorityChangedDuringValidation {
        root_index: usize,
        root: PathBuf,
        expected_canonical_root: PathBuf,
        observed_canonical_root: PathBuf,
    },
    HelperSurfaceCanonicalization {
        helper_surface: PathBuf,
        source: PathResolutionError,
    },
    HelperSurfaceOverlap {
        root_index: usize,
        root: PathBuf,
        canonical_root: PathBuf,
        helper_surface: PathBuf,
        canonical_helper_surface: PathBuf,
    },
    HelperSurfaceAuthorityChanged {
        helper_surface: PathBuf,
        expected_canonical_surface: PathBuf,
        observed_canonical_surface: PathBuf,
    },
    DuplicateCanonicalRoot {
        first_index: usize,
        first_root: PathBuf,
        second_index: usize,
        second_root: PathBuf,
        canonical_root: PathBuf,
    },
    CanonicalRootOverlap {
        first_index: usize,
        first_root: PathBuf,
        canonical_first_root: PathBuf,
        second_index: usize,
        second_root: PathBuf,
        canonical_second_root: PathBuf,
    },
    ConfiguredRootCountChanged {
        expected: usize,
        observed: usize,
    },
    RootAuthorityChanged {
        root_index: usize,
        root: PathBuf,
        expected_canonical_root: PathBuf,
        observed_canonical_root: PathBuf,
    },
    RootIdentityChanged {
        root_index: usize,
        root: PathBuf,
        canonical_root: PathBuf,
    },
}

impl fmt::Display for RoutineRootValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CurrentDirectoryUnavailable { source } => write!(
                f,
                "failed to resolve the current directory for configured QuickJS routine roots: {source}"
            ),
            Self::RuntimeRootCanonicalization {
                runtime_root,
                source,
            } => write!(
                f,
                "failed to canonicalize AgentDesk runtime-root authority `{}`: {source}",
                runtime_root.display()
            ),
            Self::RuntimeRootAuthorityChanged {
                runtime_root,
                expected_canonical_root,
                observed_canonical_root,
            } => write!(
                f,
                "AgentDesk runtime-root authority `{}` changed while routine roots were being authorized: expected `{}`, observed `{}`",
                runtime_root.display(),
                expected_canonical_root.display(),
                observed_canonical_root.display()
            ),
            Self::RootCanonicalization {
                root_index,
                root,
                source,
            } => write!(
                f,
                "failed to canonicalize configured QuickJS routine root[{root_index}] `{}`: {source}",
                root.display()
            ),
            Self::RootAuthorityChangedDuringValidation {
                root_index,
                root,
                expected_canonical_root,
                observed_canonical_root,
            } => write!(
                f,
                "configured QuickJS routine root[{root_index}] `{}` changed filesystem authority during validation: expected `{}`, observed `{}`",
                root.display(),
                expected_canonical_root.display(),
                observed_canonical_root.display()
            ),
            Self::HelperSurfaceCanonicalization {
                helper_surface,
                source,
            } => write!(
                f,
                "failed to canonicalize reserved helper surface `{}`: {source}",
                helper_surface.display()
            ),
            Self::HelperSurfaceOverlap {
                root_index,
                root,
                canonical_root,
                helper_surface,
                canonical_helper_surface,
            } => write!(
                f,
                "configured QuickJS routine root[{root_index}] `{}` resolves to `{}` and overlaps reserved runtime helper surface `{}` (canonical `{}`); configured routine roots must contain QuickJS entries only",
                root.display(),
                canonical_root.display(),
                helper_surface.display(),
                canonical_helper_surface.display()
            ),
            Self::HelperSurfaceAuthorityChanged {
                helper_surface,
                expected_canonical_surface,
                observed_canonical_surface,
            } => write!(
                f,
                "reserved runtime helper surface `{}` changed filesystem authority: expected `{}`, observed `{}`",
                helper_surface.display(),
                expected_canonical_surface.display(),
                observed_canonical_surface.display()
            ),
            Self::DuplicateCanonicalRoot {
                first_index,
                first_root,
                second_index,
                second_root,
                canonical_root,
            } => write!(
                f,
                "configured QuickJS routine roots resolve to the same canonical directory: root[{first_index}] `{}` and root[{second_index}] `{}` both resolve to `{}`",
                first_root.display(),
                second_root.display(),
                canonical_root.display()
            ),
            Self::CanonicalRootOverlap {
                first_index,
                first_root,
                canonical_first_root,
                second_index,
                second_root,
                canonical_second_root,
            } => write!(
                f,
                "configured QuickJS routine roots overlap after canonicalization: root[{first_index}] `{}` resolves to `{}`, root[{second_index}] `{}` resolves to `{}`; configured roots must be disjoint",
                first_root.display(),
                canonical_first_root.display(),
                second_root.display(),
                canonical_second_root.display()
            ),
            Self::ConfiguredRootCountChanged { expected, observed } => write!(
                f,
                "configured QuickJS routine root set changed after loader authorization: expected {expected} roots, observed {observed}"
            ),
            Self::RootAuthorityChanged {
                root_index,
                root,
                expected_canonical_root,
                observed_canonical_root,
            } => write!(
                f,
                "configured QuickJS routine root[{root_index}] `{}` changed authority after loader construction: expected `{}`, observed `{}`",
                root.display(),
                expected_canonical_root.display(),
                observed_canonical_root.display()
            ),
            Self::RootIdentityChanged {
                root_index,
                root,
                canonical_root,
            } => write!(
                f,
                "configured QuickJS routine root[{root_index}] `{}` at `{}` changed filesystem identity after loader authorization",
                root.display(),
                canonical_root.display()
            ),
        }
    }
}

impl std::error::Error for RoutineRootValidationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::CurrentDirectoryUnavailable { source } => Some(source),
            Self::RuntimeRootCanonicalization { source, .. }
            | Self::RootCanonicalization { source, .. }
            | Self::HelperSurfaceCanonicalization { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

#[cfg(unix)]
impl FileIdentity {
    fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }

    #[allow(clippy::unnecessary_cast)]
    fn from_stat(stat: &libc::stat) -> Self {
        Self {
            device: stat.st_dev as u64,
            inode: stat.st_ino as u64,
        }
    }

    fn is_available(self) -> bool {
        self.inode != 0
    }
}

#[cfg(unix)]
fn require_available_identity(identity: FileIdentity, path: &Path) -> io::Result<FileIdentity> {
    if identity.is_available() {
        return Ok(identity);
    }
    Err(io::Error::other(format!(
        "filesystem did not provide a stable inode identity for `{}`",
        path.display()
    )))
}

#[cfg(unix)]
fn checked_file_identity(
    metadata: &std::fs::Metadata,
    path: &Path,
) -> std::result::Result<FileIdentity, PathResolutionError> {
    require_available_identity(FileIdentity::from_metadata(metadata), path).map_err(|source| {
        PathResolutionError::Io {
            path: path.to_path_buf(),
            source,
        }
    })
}

#[derive(Clone, Debug)]
pub(super) struct ValidatedRoutineRoot {
    pub(super) index: usize,
    pub(super) configured: PathBuf,
    pub(super) canonical: PathBuf,
    pub(super) exists: bool,
    kind: Option<AuthorityFileKind>,
    #[cfg(unix)]
    identity: Option<FileIdentity>,
}

impl ValidatedRoutineRoot {
    pub(super) fn retains_bound_identity(&self, observed: &Self) -> bool {
        if self.canonical != observed.canonical
            || self.exists != observed.exists
            || self.kind != observed.kind
        {
            return false;
        }
        #[cfg(unix)]
        {
            self.identity == observed.identity
        }
        #[cfg(not(unix))]
        {
            let _ = observed;
            true
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct ValidatedRuntimeRoot {
    configured: PathBuf,
    canonical: PathBuf,
    exists: bool,
    kind: Option<AuthorityFileKind>,
    #[cfg(unix)]
    identity: Option<FileIdentity>,
}

impl ValidatedRuntimeRoot {
    pub(super) fn canonical(&self) -> &Path {
        &self.canonical
    }

    fn retains_bound_identity(&self, observed: &Self) -> bool {
        if self.canonical != observed.canonical
            || self.exists != observed.exists
            || self.kind != observed.kind
        {
            return false;
        }
        #[cfg(unix)]
        {
            self.identity == observed.identity
        }
        #[cfg(not(unix))]
        {
            true
        }
    }

    fn verify_observed(
        &self,
        observed: &Self,
    ) -> std::result::Result<(), RoutineRootValidationError> {
        if self.retains_bound_identity(observed) {
            return Ok(());
        }
        Err(RoutineRootValidationError::RuntimeRootAuthorityChanged {
            runtime_root: self.configured.clone(),
            expected_canonical_root: self.canonical.clone(),
            observed_canonical_root: observed.canonical.clone(),
        })
    }

    pub(super) fn verify_current(
        &self,
    ) -> std::result::Result<(), RoutineRootValidationError> {
        let observed = validate_absolute_runtime_root_authority(&self.configured)?;
        self.verify_observed(&observed)
    }
}

#[derive(Clone, Debug)]
pub(super) struct ValidatedHelperSurface {
    configured: PathBuf,
    canonical: PathBuf,
    exists: bool,
    kind: Option<AuthorityFileKind>,
    #[cfg(unix)]
    identity: Option<FileIdentity>,
}

impl ValidatedHelperSurface {
    fn retains_bound_identity(&self, observed: &Self) -> bool {
        if self.canonical != observed.canonical
            || self.exists != observed.exists
            || self.kind != observed.kind
        {
            return false;
        }
        #[cfg(unix)]
        {
            self.identity == observed.identity
        }
        #[cfg(not(unix))]
        {
            true
        }
    }

    pub(super) fn verify_observed(
        &self,
        observed: &Self,
    ) -> std::result::Result<(), RoutineRootValidationError> {
        if self.retains_bound_identity(observed) {
            return Ok(());
        }
        Err(RoutineRootValidationError::HelperSurfaceAuthorityChanged {
            helper_surface: self.configured.clone(),
            expected_canonical_surface: self.canonical.clone(),
            observed_canonical_surface: observed.canonical.clone(),
        })
    }
}

#[derive(Debug)]
pub(super) struct DiscoveredRoutineScript {
    pub(super) path: PathBuf,
    source: std::result::Result<String, RoutineSourceReadError>,
}

#[derive(Debug)]
struct RoutineSourceReadError {
    kind: io::ErrorKind,
    message: String,
}

impl From<io::Error> for RoutineSourceReadError {
    fn from(error: io::Error) -> Self {
        Self {
            kind: error.kind(),
            message: error.to_string(),
        }
    }
}

#[derive(Clone, Copy, Default)]
pub(super) struct RoutineDiscoveryHooks<'a> {
    pub(super) before_open: Option<&'a (dyn Fn(&Path) + Send + Sync)>,
    pub(super) before_read: Option<&'a (dyn Fn(&Path) + Send + Sync)>,
    pub(super) read_observer: Option<&'a (dyn Fn(&Path) + Send + Sync)>,
    pub(super) authority_check: Option<&'a (dyn Fn() -> io::Result<()> + Send + Sync)>,
}

impl DiscoveredRoutineScript {
    pub(super) fn read_source(&self) -> io::Result<String> {
        match &self.source {
            Ok(source) => Ok(source.clone()),
            Err(error) => Err(io::Error::new(error.kind, error.message.clone())),
        }
    }
}

fn read_opened_routine_source(file: &File, path: &Path) -> io::Result<String> {
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::other(format!(
            "opened routine candidate `{}` is not a regular file",
            path.display()
        )));
    }
    let mut source = String::new();
    let mut file = file;
    file.read_to_string(&mut source)?;
    Ok(source)
}

fn verify_discovery_authority(hooks: RoutineDiscoveryHooks<'_>) -> io::Result<()> {
    if let Some(check) = hooks.authority_check {
        check()?;
    }
    Ok(())
}

fn raw_absolute_path(path: &Path, current_dir: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        current_dir.join(path)
    }
}

fn canonical_identity_or_missing(
    path: &Path,
    current_dir: &Path,
) -> std::result::Result<PathBuf, PathResolutionError> {
    let absolute = raw_absolute_path(path, current_dir);
    let mut probe = absolute.clone();
    let mut missing_tail = Vec::new();

    loop {
        match probe.canonicalize() {
            Ok(mut canonical) => {
                for component in missing_tail.iter().rev() {
                    canonical.push(component);
                }
                return Ok(canonical);
            }
            Err(canonical_error) if canonical_error.kind() == io::ErrorKind::NotFound => {
                match std::fs::symlink_metadata(&probe) {
                    Ok(metadata) if metadata.file_type().is_symlink() => {
                        return Err(PathResolutionError::DanglingSymlink { path: probe });
                    }
                    Ok(_) => {
                        return Err(PathResolutionError::Io {
                            path: probe,
                            source: canonical_error,
                        });
                    }
                    Err(metadata_error) if metadata_error.kind() == io::ErrorKind::NotFound => {}
                    Err(source) => {
                        return Err(PathResolutionError::Io {
                            path: probe,
                            source,
                        });
                    }
                }

                match probe.components().next_back() {
                    Some(Component::Normal(component)) => {
                        missing_tail.push(component.to_os_string());
                        if !probe.pop() {
                            return Err(PathResolutionError::Io {
                                path: absolute,
                                source: canonical_error,
                            });
                        }
                    }
                    Some(Component::CurDir) => {
                        if !probe.pop() {
                            return Err(PathResolutionError::Io {
                                path: absolute,
                                source: canonical_error,
                            });
                        }
                    }
                    Some(Component::ParentDir) => {
                        return Err(PathResolutionError::AmbiguousMissingPath { path: absolute });
                    }
                    Some(Component::RootDir | Component::Prefix(_)) | None => {
                        return Err(PathResolutionError::Io {
                            path: absolute,
                            source: canonical_error,
                        });
                    }
                }
            }
            Err(source) => {
                return Err(PathResolutionError::Io {
                    path: probe,
                    source,
                });
            }
        }
    }
}

fn paths_overlap(first: &Path, second: &Path) -> bool {
    first == second || first.starts_with(second) || second.starts_with(first)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AuthorityFileKind {
    Directory,
    RegularFile,
    Symlink,
    Other,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ResolvedPathAuthority {
    canonical: PathBuf,
    exists: bool,
    kind: Option<AuthorityFileKind>,
    #[cfg(unix)]
    identity: Option<FileIdentity>,
}

fn resolve_path_authority(
    configured: &Path,
    current_dir: &Path,
) -> std::result::Result<ResolvedPathAuthority, PathResolutionError> {
    let canonical = canonical_identity_or_missing(configured, current_dir)?;
    let metadata = match std::fs::symlink_metadata(&canonical) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(source) => {
            return Err(PathResolutionError::Io {
                path: canonical,
                source,
            });
        }
    };
    let kind = metadata.as_ref().map(|metadata| {
        let file_type = metadata.file_type();
        if file_type.is_dir() {
            AuthorityFileKind::Directory
        } else if file_type.is_file() {
            AuthorityFileKind::RegularFile
        } else if file_type.is_symlink() {
            AuthorityFileKind::Symlink
        } else {
            AuthorityFileKind::Other
        }
    });
    #[cfg(unix)]
    let identity = metadata
        .as_ref()
        .map(|metadata| checked_file_identity(metadata, &canonical))
        .transpose()?;
    Ok(ResolvedPathAuthority {
        canonical,
        exists: metadata.is_some(),
        kind,
        #[cfg(unix)]
        identity,
    })
}

fn validate_absolute_runtime_root_authority(
    absolute_runtime_root: &Path,
) -> std::result::Result<ValidatedRuntimeRoot, RoutineRootValidationError> {
    debug_assert!(absolute_runtime_root.is_absolute());
    let current_dir_is_unused_for_absolute_path = Path::new("");
    let expected = resolve_path_authority(
        absolute_runtime_root,
        current_dir_is_unused_for_absolute_path,
    )
    .map_err(|source| RoutineRootValidationError::RuntimeRootCanonicalization {
        runtime_root: absolute_runtime_root.to_path_buf(),
        source,
    })?;
    let observed = resolve_path_authority(
        absolute_runtime_root,
        current_dir_is_unused_for_absolute_path,
    )
    .map_err(|source| RoutineRootValidationError::RuntimeRootCanonicalization {
        runtime_root: absolute_runtime_root.to_path_buf(),
        source,
    })?;
    if expected != observed {
        return Err(RoutineRootValidationError::RuntimeRootAuthorityChanged {
            runtime_root: absolute_runtime_root.to_path_buf(),
            expected_canonical_root: expected.canonical,
            observed_canonical_root: observed.canonical,
        });
    }
    Ok(ValidatedRuntimeRoot {
        configured: absolute_runtime_root.to_path_buf(),
        canonical: expected.canonical,
        exists: expected.exists,
        kind: expected.kind,
        #[cfg(unix)]
        identity: expected.identity,
    })
}

fn validate_routine_authority_inner<F>(
    roots: &[PathBuf],
    runtime_root: &Path,
    current_dir_override: Option<&Path>,
    mut after_first_root_snapshot: F,
) -> std::result::Result<
    (Vec<ValidatedRoutineRoot>, ValidatedHelperSurface),
    RoutineRootValidationError,
>
where
    F: FnMut(usize),
{
    let current_dir = match current_dir_override {
        Some(current_dir) => current_dir.to_path_buf(),
        None => std::env::current_dir().map_err(|source| {
            RoutineRootValidationError::CurrentDirectoryUnavailable { source }
        })?,
    };
    let mut identities = Vec::with_capacity(roots.len());
    for (index, root) in roots.iter().enumerate() {
        let expected = resolve_path_authority(root, &current_dir).map_err(|source| {
            RoutineRootValidationError::RootCanonicalization {
                root_index: index,
                root: root.clone(),
                source,
            }
        })?;
        after_first_root_snapshot(index);
        let observed = resolve_path_authority(root, &current_dir).map_err(|source| {
            RoutineRootValidationError::RootCanonicalization {
                root_index: index,
                root: root.clone(),
                source,
            }
        })?;
        if expected != observed {
            return Err(
                RoutineRootValidationError::RootAuthorityChangedDuringValidation {
                    root_index: index,
                    root: root.clone(),
                    expected_canonical_root: expected.canonical,
                    observed_canonical_root: observed.canonical,
                },
            );
        }
        identities.push(ValidatedRoutineRoot {
            index,
            configured: root.clone(),
            canonical: expected.canonical,
            exists: expected.exists,
            kind: expected.kind,
            #[cfg(unix)]
            identity: expected.identity,
        });
    }

    let helper_surface = runtime_root.join(ROUTINE_HELPERS_DIR_NAME);
    let expected_helper = resolve_path_authority(&helper_surface, &current_dir).map_err(|source| {
        RoutineRootValidationError::HelperSurfaceCanonicalization {
            helper_surface: helper_surface.clone(),
            source,
        }
    })?;
    let observed_helper =
        resolve_path_authority(&helper_surface, &current_dir).map_err(|source| {
            RoutineRootValidationError::HelperSurfaceCanonicalization {
                helper_surface: helper_surface.clone(),
                source,
            }
        })?;
    if expected_helper != observed_helper {
        return Err(RoutineRootValidationError::HelperSurfaceAuthorityChanged {
            helper_surface: helper_surface.clone(),
            expected_canonical_surface: expected_helper.canonical,
            observed_canonical_surface: observed_helper.canonical,
        });
    }
    let canonical_helper_surface = expected_helper.canonical;
    let helper_authority = ValidatedHelperSurface {
        configured: helper_surface.clone(),
        canonical: canonical_helper_surface.clone(),
        exists: expected_helper.exists,
        kind: expected_helper.kind,
        #[cfg(unix)]
        identity: expected_helper.identity,
    };
    for root in &identities {
        let aliases_helper_identity = {
            #[cfg(unix)]
            {
                root.identity.is_some() && root.identity == helper_authority.identity
            }
            #[cfg(not(unix))]
            {
                false
            }
        };
        if paths_overlap(&root.canonical, &canonical_helper_surface)
            || aliases_helper_identity
        {
            return Err(RoutineRootValidationError::HelperSurfaceOverlap {
                root_index: root.index,
                root: root.configured.clone(),
                canonical_root: root.canonical.clone(),
                helper_surface: helper_surface.clone(),
                canonical_helper_surface: canonical_helper_surface.clone(),
            });
        }
    }

    for (position, first) in identities.iter().enumerate() {
        for second in identities.iter().skip(position + 1) {
            if first.canonical == second.canonical {
                return Err(RoutineRootValidationError::DuplicateCanonicalRoot {
                    first_index: first.index,
                    first_root: first.configured.clone(),
                    second_index: second.index,
                    second_root: second.configured.clone(),
                    canonical_root: first.canonical.clone(),
                });
            }
            if paths_overlap(&first.canonical, &second.canonical) {
                return Err(RoutineRootValidationError::CanonicalRootOverlap {
                    first_index: first.index,
                    first_root: first.configured.clone(),
                    canonical_first_root: first.canonical.clone(),
                    second_index: second.index,
                    second_root: second.configured.clone(),
                    canonical_second_root: second.canonical.clone(),
                });
            }
        }
    }

    Ok((identities, helper_authority))
}

pub(super) fn validate_routine_authority(
    roots: &[PathBuf],
    runtime_root: &Path,
    current_dir_override: Option<&Path>,
) -> std::result::Result<
    (Vec<ValidatedRoutineRoot>, ValidatedHelperSurface),
    RoutineRootValidationError,
> {
    validate_routine_authority_inner(roots, runtime_root, current_dir_override, |_| {})
}

#[cfg(test)]
pub(super) fn validate_routine_authority_with_hook<F>(
    roots: &[PathBuf],
    runtime_root: &Path,
    current_dir_override: Option<&Path>,
    after_first_root_snapshot: F,
) -> std::result::Result<
    (Vec<ValidatedRoutineRoot>, ValidatedHelperSurface),
    RoutineRootValidationError,
>
where
    F: FnMut(usize),
{
    validate_routine_authority_inner(
        roots,
        runtime_root,
        current_dir_override,
        after_first_root_snapshot,
    )
}

#[cfg(test)]
pub(super) fn validate_routine_roots(
    roots: &[PathBuf],
    runtime_root: &Path,
    current_dir_override: Option<&Path>,
) -> std::result::Result<Vec<ValidatedRoutineRoot>, RoutineRootValidationError> {
    validate_routine_authority(roots, runtime_root, current_dir_override).map(|(roots, _)| roots)
}

fn bind_routine_root_authority_inner<F>(
    roots: &[PathBuf],
    runtime_root: &Path,
    current_dir_override: Option<&Path>,
    after_runtime_root_resolve: F,
) -> std::result::Result<
    (
        ValidatedRuntimeRoot,
        Vec<ValidatedRoutineRoot>,
        ValidatedHelperSurface,
    ),
    RoutineRootValidationError,
>
where
    F: FnOnce(),
{
    let current_dir = match current_dir_override {
        Some(current_dir) => current_dir.to_path_buf(),
        None => std::env::current_dir().map_err(|source| {
            RoutineRootValidationError::CurrentDirectoryUnavailable { source }
        })?,
    };
    let absolute_runtime_root = raw_absolute_path(runtime_root, &current_dir);
    let initial_runtime_authority =
        validate_absolute_runtime_root_authority(&absolute_runtime_root)?;
    let canonical_runtime_root = initial_runtime_authority.canonical().to_path_buf();
    let (_, initial_helper_authority) =
        validate_routine_authority(&[], &canonical_runtime_root, Some(&current_dir))?;

    after_runtime_root_resolve();

    // Config resolution joins runtime-relative roots to the raw runtime-root
    // path. Rebase those descendants onto the already-resolved authority so a
    // symlink alias cannot point the root and helper checks at different
    // releases during one constructor call.
    let authority_bound_roots = roots
        .iter()
        .map(|root| {
            let absolute_root = raw_absolute_path(root, &current_dir);
            absolute_root
                .strip_prefix(&absolute_runtime_root)
                .map(|suffix| canonical_runtime_root.join(suffix))
                .unwrap_or(absolute_root)
        })
        .collect::<Vec<_>>();
    let (mut validated_roots, helper_authority) = validate_routine_authority(
        &authority_bound_roots,
        &canonical_runtime_root,
        Some(&current_dir),
    )?;
    initial_helper_authority.verify_observed(&helper_authority)?;
    for (validated, configured) in validated_roots.iter_mut().zip(roots) {
        validated.configured = configured.clone();
    }

    let observed_runtime_authority =
        validate_absolute_runtime_root_authority(&absolute_runtime_root)?;
    initial_runtime_authority.verify_observed(&observed_runtime_authority)?;
    let (_, final_helper_authority) =
        validate_routine_authority(&[], &canonical_runtime_root, Some(&current_dir))?;
    initial_helper_authority.verify_observed(&final_helper_authority)?;
    let final_runtime_authority =
        validate_absolute_runtime_root_authority(&absolute_runtime_root)?;
    initial_runtime_authority.verify_observed(&final_runtime_authority)?;

    Ok((
        initial_runtime_authority,
        validated_roots,
        initial_helper_authority,
    ))
}

pub(super) fn bind_routine_root_authority(
    roots: &[PathBuf],
    runtime_root: &Path,
) -> std::result::Result<
    (
        ValidatedRuntimeRoot,
        Vec<ValidatedRoutineRoot>,
        ValidatedHelperSurface,
    ),
    RoutineRootValidationError,
> {
    bind_routine_root_authority_inner(roots, runtime_root, None, || {})
}

#[cfg(test)]
pub(super) fn bind_routine_root_authority_with_hook<F>(
    roots: &[PathBuf],
    runtime_root: &Path,
    after_runtime_root_resolve: F,
) -> std::result::Result<
    (
        ValidatedRuntimeRoot,
        Vec<ValidatedRoutineRoot>,
        ValidatedHelperSurface,
    ),
    RoutineRootValidationError,
>
where
    F: FnOnce(),
{
    bind_routine_root_authority_inner(
        roots,
        runtime_root,
        None,
        after_runtime_root_resolve,
    )
}

#[cfg(test)]
pub(super) fn candidate_failure_key(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .expect("test current directory must be available")
                .join(path)
        }
    })
}

pub(super) fn routine_roots_identity(
    runtime_authority: &ValidatedRuntimeRoot,
    roots: &[ValidatedRoutineRoot],
    helper_authority: &ValidatedHelperSurface,
) -> PathBuf {
    use sha2::{Digest as _, Sha256};

    fn update_path(hasher: &mut Sha256, path: &Path) {
        let bytes = path.as_os_str().as_encoded_bytes();
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    }

    fn update_identity(
        hasher: &mut Sha256,
        exists: bool,
        kind: Option<AuthorityFileKind>,
        identity: Option<(u64, u64)>,
    ) {
        hasher.update([u8::from(exists)]);
        hasher.update([match kind {
            None => 0,
            Some(AuthorityFileKind::Directory) => 1,
            Some(AuthorityFileKind::RegularFile) => 2,
            Some(AuthorityFileKind::Symlink) => 3,
            Some(AuthorityFileKind::Other) => 4,
        }]);
        match identity {
            Some((device, inode)) => {
                hasher.update([1]);
                hasher.update(device.to_le_bytes());
                hasher.update(inode.to_le_bytes());
            }
            None => hasher.update([0]),
        }
    }

    let mut hasher = Sha256::new();
    hasher.update(b"runtime");
    update_path(&mut hasher, &runtime_authority.configured);
    update_path(&mut hasher, &runtime_authority.canonical);
    #[cfg(unix)]
    let runtime_identity = runtime_authority
        .identity
        .map(|identity| (identity.device, identity.inode));
    #[cfg(not(unix))]
    let runtime_identity = None;
    update_identity(
        &mut hasher,
        runtime_authority.exists,
        runtime_authority.kind,
        runtime_identity,
    );
    for root in roots {
        hasher.update(b"root");
        update_path(&mut hasher, &root.canonical);
        #[cfg(unix)]
        let identity = root
            .identity
            .map(|identity| (identity.device, identity.inode));
        #[cfg(not(unix))]
        let identity = None;
        update_identity(&mut hasher, root.exists, root.kind, identity);
    }
    hasher.update(b"helper");
    update_path(&mut hasher, &helper_authority.canonical);
    #[cfg(unix)]
    let helper_identity = helper_authority
        .identity
        .map(|identity| (identity.device, identity.inode));
    #[cfg(not(unix))]
    let helper_identity = None;
    update_identity(
        &mut hasher,
        helper_authority.exists,
        helper_authority.kind,
        helper_identity,
    );
    PathBuf::from(hex::encode(hasher.finalize()))
}

pub(super) fn script_ref(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

pub(super) fn add_cached_candidates_for_root(
    existing_scripts: &HashMap<String, LoadedRoutineScript>,
    candidates_by_ref: &mut BTreeMap<String, Vec<RoutineScriptCandidate>>,
    seen_refs: &mut HashSet<String>,
    root_index: usize,
    root: &Path,
) {
    for (script_ref, script) in existing_scripts
        .iter()
        .filter(|(_, script)| script.file.starts_with(root))
    {
        seen_refs.insert(script_ref.clone());
        candidates_by_ref
            .entry(script_ref.clone())
            .or_default()
            .push(RoutineScriptCandidate {
                root_index,
                root: root.to_path_buf(),
                path: script.file.clone(),
                failure_key: script.file.clone(),
                snapshot: None,
                cached: Some(script.clone()),
            });
    }
}

#[cfg(unix)]
struct DirectoryStream(*mut libc::DIR);

#[cfg(unix)]
struct PinnedDirectoryEntry {
    name: OsString,
    identity: FileIdentity,
    kind: PinnedEntryKind,
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PinnedEntryKind {
    Directory,
    RegularFile,
    Other,
}

#[cfg(unix)]
impl Drop for DirectoryStream {
    fn drop(&mut self) {
        // SAFETY: `DirectoryStream` exclusively owns the successful `fdopendir` result.
        unsafe {
            libc::closedir(self.0);
        }
    }
}

#[cfg(unix)]
fn directory_entries(directory: &File) -> io::Result<Vec<PinnedDirectoryEntry>> {
    // SAFETY: `fcntl` receives a valid live descriptor and returns an independent descriptor.
    let duplicate = unsafe { libc::fcntl(directory.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
    if duplicate < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `duplicate` is an owned directory descriptor. `fdopendir` owns it on success.
    let stream = unsafe { libc::fdopendir(duplicate) };
    if stream.is_null() {
        let error = io::Error::last_os_error();
        // SAFETY: `fdopendir` failed, so ownership of `duplicate` remains here.
        unsafe {
            libc::close(duplicate);
        }
        return Err(error);
    }
    let stream = DirectoryStream(stream);
    let mut entries = Vec::new();
    loop {
        clear_readdir_errno();
        // SAFETY: the stream stays live for this call and the returned entry is copied immediately.
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            if let Some(error) = readdir_error() {
                return Err(error);
            }
            break;
        }
        // SAFETY: POSIX guarantees `d_name` is a NUL-terminated byte sequence in this entry.
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        let bytes = name.to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        let name = OsStr::from_bytes(bytes).to_os_string();
        let (identity, kind) = entry_identity(directory, &name)?;
        entries.push(PinnedDirectoryEntry {
            name,
            identity,
            kind,
        });
    }
    Ok(entries)
}

#[cfg(unix)]
fn clear_readdir_errno() {
    #[cfg(any(target_os = "linux", target_os = "dragonfly"))]
    // SAFETY: libc exposes the calling thread's errno slot through this pointer.
    unsafe {
        *libc::__errno_location() = 0;
    }
    #[cfg(any(
        target_os = "android",
        target_os = "netbsd",
        target_os = "openbsd"
    ))]
    // SAFETY: libc exposes the calling thread's errno slot through this pointer.
    unsafe {
        *libc::__errno() = 0;
    }
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd"
    ))]
    // SAFETY: libc exposes the calling thread's errno slot through this pointer.
    unsafe {
        *libc::__error() = 0;
    }
}

#[cfg(unix)]
fn readdir_error() -> Option<io::Error> {
    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    ))]
    {
        let error = io::Error::last_os_error();
        return (error.raw_os_error() != Some(0)).then_some(error);
    }
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    )))]
    None
}

#[cfg(unix)]
fn directory_entry_cstring(name: &OsStr) -> io::Result<CString> {
    CString::new(name.as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "routine directory entry contains an interior NUL",
        )
    })
}

#[cfg(unix)]
fn entry_identity(parent: &File, name: &OsStr) -> io::Result<(FileIdentity, PinnedEntryKind)> {
    let entry_path = PathBuf::from(name);
    let name = directory_entry_cstring(name)?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `parent` and `name` stay live, and `stat` points to writable storage.
    let result = unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful `fstatat` initialized the complete `stat` value.
    let stat = unsafe { stat.assume_init() };
    let kind = match stat.st_mode & libc::S_IFMT {
        libc::S_IFDIR => PinnedEntryKind::Directory,
        libc::S_IFREG => PinnedEntryKind::RegularFile,
        _ => PinnedEntryKind::Other,
    };
    let identity = require_available_identity(FileIdentity::from_stat(&stat), &entry_path)?;
    Ok((identity, kind))
}

#[cfg(unix)]
fn openat(parent: &File, name: &OsStr, flags: libc::c_int) -> io::Result<File> {
    let name = directory_entry_cstring(name)?;
    // SAFETY: `parent` is live and `name` is NUL-terminated for the duration of the call.
    let descriptor = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a successful `openat` returns a new descriptor owned by this function.
    Ok(unsafe { File::from_raw_fd(descriptor) })
}

#[cfg(unix)]
fn open_root(root: &ValidatedRoutineRoot) -> io::Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).custom_flags(
        libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
    );
    let directory = options.open(&root.canonical)?;
    let metadata = directory.metadata()?;
    let observed_identity =
        require_available_identity(FileIdentity::from_metadata(&metadata), &root.canonical)?;
    if !metadata.is_dir()
        || root
            .identity
            .is_some_and(|identity| identity != observed_identity)
    {
        return Err(io::Error::other(format!(
            "validated routine root `{}` no longer names the preflight directory",
            root.canonical.display()
        )));
    }
    Ok(directory)
}

#[cfg(unix)]
fn verify_opened_entry_identity(
    entry: &PinnedDirectoryEntry,
    opened: &File,
    path: &Path,
) -> io::Result<()> {
    let opened_metadata = opened.metadata()?;
    let opened_identity =
        require_available_identity(FileIdentity::from_metadata(&opened_metadata), path)?;
    if opened_identity != entry.identity {
        return Err(io::Error::other(format!(
            "routine entry `{}` changed identity during discovery",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(unix)]
fn collect_routine_scripts_inner(
    directory: &File,
    current_path: &Path,
    hooks: RoutineDiscoveryHooks<'_>,
    out: &mut Vec<DiscoveredRoutineScript>,
) -> io::Result<()> {
    let mut entries = directory_entries(directory)?;
    entries.sort_by(|first, second| first.name.cmp(&second.name));
    for entry in entries {
        let path = current_path.join(&entry.name);
        match entry.kind {
            PinnedEntryKind::Directory => {
                if let Some(hook) = hooks.before_open {
                    hook(&path);
                }
                verify_discovery_authority(hooks)?;
                let child_directory = openat(
                    directory,
                    &entry.name,
                    libc::O_RDONLY
                        | libc::O_DIRECTORY
                        | libc::O_NOFOLLOW
                        | libc::O_NONBLOCK
                        | libc::O_CLOEXEC,
                )?;
                verify_opened_entry_identity(&entry, &child_directory, &path)?;
                verify_discovery_authority(hooks)?;
                collect_routine_scripts_inner(&child_directory, &path, hooks, out)?;
            }
            PinnedEntryKind::RegularFile
                if path.extension().is_some_and(|extension| extension == "js") =>
            {
                if let Some(hook) = hooks.before_open {
                    hook(&path);
                }
                verify_discovery_authority(hooks)?;
                let file = openat(
                    directory,
                    &entry.name,
                    libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
                )?;
                verify_opened_entry_identity(&entry, &file, &path)?;
                if let Some(hook) = hooks.before_read {
                    hook(&path);
                }
                verify_discovery_authority(hooks)?;
                if let Some(observer) = hooks.read_observer {
                    observer(&path);
                }
                verify_discovery_authority(hooks)?;
                let source = read_opened_routine_source(&file, &path)
                    .map_err(RoutineSourceReadError::from);
                verify_discovery_authority(hooks)?;
                out.push(DiscoveredRoutineScript { path, source });
            }
            PinnedEntryKind::RegularFile | PinnedEntryKind::Other => {}
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn collect_routine_scripts_inner(
    current_path: &Path,
    hooks: RoutineDiscoveryHooks<'_>,
    out: &mut Vec<DiscoveredRoutineScript>,
) -> io::Result<()> {
    let mut entries = std::fs::read_dir(current_path)?.collect::<io::Result<Vec<_>>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        if let Some(hook) = hooks.before_open {
            hook(&path);
        }
        verify_discovery_authority(hooks)?;
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_routine_scripts_inner(&path, hooks, out)?;
            continue;
        }
        if !file_type.is_file() || path.extension().is_none_or(|extension| extension != "js") {
            continue;
        }
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(windows)]
        options.custom_flags(0x0020_0000);
        let file = options.open(&path)?;
        if !file.metadata()?.is_file() || std::fs::symlink_metadata(&path)?.file_type().is_symlink()
        {
            return Err(io::Error::other(format!(
                "routine candidate `{}` is not a non-reparse regular file",
                path.display()
            )));
        }
        if let Some(hook) = hooks.before_read {
            hook(&path);
        }
        verify_discovery_authority(hooks)?;
        if let Some(observer) = hooks.read_observer {
            observer(&path);
        }
        verify_discovery_authority(hooks)?;
        let source =
            read_opened_routine_source(&file, &path).map_err(RoutineSourceReadError::from);
        verify_discovery_authority(hooks)?;
        out.push(DiscoveredRoutineScript { path, source });
    }
    Ok(())
}

pub(super) fn collect_routine_script_paths(
    root: &ValidatedRoutineRoot,
    hooks: RoutineDiscoveryHooks<'_>,
) -> io::Result<Vec<DiscoveredRoutineScript>> {
    #[cfg(unix)]
    {
        verify_discovery_authority(hooks)?;
        let directory = open_root(root)?;
        verify_discovery_authority(hooks)?;
        let mut out = Vec::new();
        collect_routine_scripts_inner(&directory, &root.canonical, hooks, &mut out)?;
        verify_discovery_authority(hooks)?;
        Ok(out)
    }
    #[cfg(not(unix))]
    {
        verify_discovery_authority(hooks)?;
        let mut out = Vec::new();
        collect_routine_scripts_inner(&root.canonical, hooks, &mut out)?;
        verify_discovery_authority(hooks)?;
        Ok(out)
    }
}
