use super::{LoadedRoutineScript, RoutineScriptCandidate, full_source_version};
use anyhow::Result;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::path::{Component, Path, PathBuf};

const ROUTINE_HELPERS_DIR_NAME: &str = "routine-helpers";

#[derive(Debug)]
pub(super) enum RoutineRootValidationError {
    RootCanonicalization {
        root_index: usize,
        root: PathBuf,
        source: std::io::Error,
    },
    HelperSurfaceCanonicalization {
        helper_surface: PathBuf,
        source: std::io::Error,
    },
    PrimaryRootHasNoParent {
        root: PathBuf,
        canonical_root: PathBuf,
    },
    HelperSurfaceOverlap {
        root_index: usize,
        root: PathBuf,
        canonical_root: PathBuf,
        helper_surface: PathBuf,
        canonical_helper_surface: PathBuf,
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
}

impl fmt::Display for RoutineRootValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RootCanonicalization {
                root_index,
                root,
                source,
            } => write!(
                f,
                "failed to canonicalize configured QuickJS routine root[{root_index}] `{}`: {source}",
                root.display()
            ),
            Self::HelperSurfaceCanonicalization {
                helper_surface,
                source,
            } => write!(
                f,
                "failed to canonicalize reserved sibling helper surface `{}`: {source}",
                helper_surface.display()
            ),
            Self::PrimaryRootHasNoParent {
                root,
                canonical_root,
            } => write!(
                f,
                "configured primary QuickJS routine root `{}` resolves to `{}` and has no parent from which to derive the sibling `{ROUTINE_HELPERS_DIR_NAME}` surface",
                root.display(),
                canonical_root.display()
            ),
            Self::HelperSurfaceOverlap {
                root_index,
                root,
                canonical_root,
                helper_surface,
                canonical_helper_surface,
            } => write!(
                f,
                "configured QuickJS routine root[{root_index}] `{}` resolves to `{}` and overlaps reserved sibling helper surface `{}` (canonical `{}`); configured routine roots must contain QuickJS entries only",
                root.display(),
                canonical_root.display(),
                helper_surface.display(),
                canonical_helper_surface.display()
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
        }
    }
}

impl std::error::Error for RoutineRootValidationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::RootCanonicalization { source, .. }
            | Self::HelperSurfaceCanonicalization { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[derive(Debug)]
struct ConfiguredRootIdentity {
    index: usize,
    configured: PathBuf,
    canonical: PathBuf,
}

pub(super) fn stable_absolute_path(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn root_config_identity(root: &Path) -> PathBuf {
    stable_absolute_path(root)
}

fn canonical_identity_or_missing(path: &Path) -> std::io::Result<PathBuf> {
    match path.canonicalize() {
        Ok(canonical) => return Ok(canonical),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let absolute = stable_absolute_path(path);
    let mut probe = absolute.as_path();
    let mut missing_tail = Vec::new();
    loop {
        match probe.canonicalize() {
            Ok(mut canonical) => {
                for component in missing_tail.iter().rev() {
                    canonical.push(component);
                }
                return Ok(canonical);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let Some(component) = probe.file_name() else {
                    return Ok(absolute);
                };
                missing_tail.push(component.to_os_string());
                let Some(parent) = probe.parent() else {
                    return Ok(absolute);
                };
                probe = parent;
            }
            Err(error) => return Err(error),
        }
    }
}

fn paths_overlap(first: &Path, second: &Path) -> bool {
    first == second || first.starts_with(second) || second.starts_with(first)
}

pub(super) fn validate_routine_roots(
    roots: &[PathBuf],
) -> std::result::Result<(), RoutineRootValidationError> {
    let mut identities = Vec::with_capacity(roots.len());
    for (index, root) in roots.iter().enumerate() {
        let canonical = canonical_identity_or_missing(root).map_err(|source| {
            RoutineRootValidationError::RootCanonicalization {
                root_index: index,
                root: root.clone(),
                source,
            }
        })?;
        identities.push(ConfiguredRootIdentity {
            index,
            configured: root.clone(),
            canonical,
        });
    }

    let Some(primary) = identities.first() else {
        return Ok(());
    };
    let Some(primary_parent) = primary.canonical.parent() else {
        return Err(RoutineRootValidationError::PrimaryRootHasNoParent {
            root: primary.configured.clone(),
            canonical_root: primary.canonical.clone(),
        });
    };
    let helper_surface = primary_parent.join(ROUTINE_HELPERS_DIR_NAME);
    let canonical_helper_surface = canonical_identity_or_missing(&helper_surface).map_err(|source| {
        RoutineRootValidationError::HelperSurfaceCanonicalization {
            helper_surface: helper_surface.clone(),
            source,
        }
    })?;

    for root in &identities {
        if paths_overlap(&root.canonical, &canonical_helper_surface) {
            return Err(RoutineRootValidationError::HelperSurfaceOverlap {
                root_index: root.index,
                root: root.configured.clone(),
                canonical_root: root.canonical.clone(),
                helper_surface,
                canonical_helper_surface,
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

    Ok(())
}

pub(super) fn candidate_failure_key(path: &Path) -> PathBuf {
    path.canonicalize()
        .unwrap_or_else(|_| stable_absolute_path(path))
}

pub(super) fn routine_roots_identity(roots: &[PathBuf]) -> PathBuf {
    let identities = roots
        .iter()
        .map(|root| root_config_identity(root).to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("\0");
    PathBuf::from(full_source_version(&identities))
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
                cached: Some(script.clone()),
            });
    }
}

pub(super) fn collect_routine_script_paths(root: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    collect_routine_script_paths_inner(root, out)
}

fn collect_routine_script_paths_inner(current_dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(current_dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let path = entry.path();
        if file_type.is_dir() {
            collect_routine_script_paths_inner(&path, out)?;
        } else if file_type.is_file() && path.extension().is_some_and(|ext| ext == "js") {
            out.push(path);
        }
    }
    Ok(())
}
