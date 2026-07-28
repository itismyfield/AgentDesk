use std::fmt;
use std::io;
use std::path::{Component, Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt as _;

const ROUTINE_HELPERS_DIR_NAME: &str = "routine-helpers";

#[derive(Debug)]
pub(in super::super) enum PathResolutionError {
    Io { path: PathBuf, source: io::Error },
    DanglingSymlink { path: PathBuf },
    AmbiguousMissingPath { path: PathBuf },
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
pub(in super::super) enum RoutineRootValidationError {
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
pub(super) struct FileIdentity {
    pub(super) device: u64,
    pub(super) inode: u64,
}

#[cfg(unix)]
impl FileIdentity {
    pub(super) fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }

    #[allow(clippy::unnecessary_cast)]
    pub(super) fn from_stat(stat: &libc::stat) -> Self {
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
pub(super) fn require_available_identity(
    identity: FileIdentity,
    path: &Path,
) -> io::Result<FileIdentity> {
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
pub(in super::super) struct ValidatedRoutineRoot {
    pub(in super::super) index: usize,
    pub(in super::super) configured: PathBuf,
    pub(in super::super) canonical: PathBuf,
    pub(in super::super) exists: bool,
    pub(super) kind: Option<AuthorityFileKind>,
    #[cfg(unix)]
    pub(super) identity: Option<FileIdentity>,
}

impl ValidatedRoutineRoot {
    pub(in super::super) fn retains_bound_identity(&self, observed: &Self) -> bool {
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
pub(in super::super) struct ValidatedRuntimeRoot {
    pub(super) configured: PathBuf,
    pub(super) canonical: PathBuf,
    pub(super) exists: bool,
    pub(super) kind: Option<AuthorityFileKind>,
    #[cfg(unix)]
    pub(super) identity: Option<FileIdentity>,
}

impl ValidatedRuntimeRoot {
    pub(in super::super) fn canonical(&self) -> &Path {
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

    pub(in super::super) fn verify_current(
        &self,
    ) -> std::result::Result<(), RoutineRootValidationError> {
        let observed = validate_absolute_runtime_root_authority(&self.configured)?;
        self.verify_observed(&observed)
    }
}

#[derive(Clone, Debug)]
pub(in super::super) struct ValidatedHelperSurface {
    configured: PathBuf,
    pub(super) canonical: PathBuf,
    pub(super) exists: bool,
    pub(super) kind: Option<AuthorityFileKind>,
    #[cfg(unix)]
    pub(super) identity: Option<FileIdentity>,
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

    pub(in super::super) fn verify_observed(
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
pub(super) enum AuthorityFileKind {
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
    .map_err(
        |source| RoutineRootValidationError::RuntimeRootCanonicalization {
            runtime_root: absolute_runtime_root.to_path_buf(),
            source,
        },
    )?;
    let observed = resolve_path_authority(
        absolute_runtime_root,
        current_dir_is_unused_for_absolute_path,
    )
    .map_err(
        |source| RoutineRootValidationError::RuntimeRootCanonicalization {
            runtime_root: absolute_runtime_root.to_path_buf(),
            source,
        },
    )?;
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
        None => std::env::current_dir()
            .map_err(|source| RoutineRootValidationError::CurrentDirectoryUnavailable { source })?,
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
    let expected_helper =
        resolve_path_authority(&helper_surface, &current_dir).map_err(|source| {
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
        if paths_overlap(&root.canonical, &canonical_helper_surface) || aliases_helper_identity {
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

pub(in super::super) fn validate_routine_authority(
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
pub(in super::super) fn validate_routine_authority_with_hook<F>(
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
pub(in super::super) fn validate_routine_roots(
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
        None => std::env::current_dir()
            .map_err(|source| RoutineRootValidationError::CurrentDirectoryUnavailable { source })?,
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
    let final_runtime_authority = validate_absolute_runtime_root_authority(&absolute_runtime_root)?;
    initial_runtime_authority.verify_observed(&final_runtime_authority)?;

    Ok((
        initial_runtime_authority,
        validated_roots,
        initial_helper_authority,
    ))
}

pub(in super::super) fn bind_routine_root_authority(
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
pub(in super::super) fn bind_routine_root_authority_with_hook<F>(
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
    bind_routine_root_authority_inner(roots, runtime_root, None, after_runtime_root_resolve)
}
