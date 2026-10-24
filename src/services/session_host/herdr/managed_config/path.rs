use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Component, Path, PathBuf};

use super::invalid;
use crate::config::session_hosts::{BootSessionHosts, EndpointMode, SessionHostsConfig};
use crate::services::discord::runtime_store::fsync_parent_dir;

#[derive(Debug)]
pub(crate) struct PersonalPaths {
    pub home: PathBuf,
    pub xdg: Option<PathBuf>,
}

impl PersonalPaths {
    /// The executing account's home and XDG config home; an unknown home authorizes nothing.
    pub(crate) fn current() -> Self {
        Self {
            home: dirs::home_dir().unwrap_or_default(),
            xdg: std::env::var_os("XDG_CONFIG_HOME")
                .filter(|xdg| !xdg.is_empty())
                .map(PathBuf::from),
        }
    }
}

/// The endpoints that share one managed home, or why they may not be written.
#[derive(Debug)]
pub(crate) struct Target {
    pub keys: Vec<String>,
    pub declared: PathBuf,
    pub home: io::Result<ManagedHome>,
}

#[derive(Debug)]
pub(crate) struct ManagedHome {
    pub(super) path: PathBuf,
    pub(super) uid: u32,
}

fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

fn protect(path: &Path, personal: &PersonalPaths) -> io::Result<()> {
    if !personal.home.is_absolute() || personal.xdg.as_ref().is_some_and(|xdg| !xdg.is_absolute()) {
        return Err(invalid("personal herdr boundaries must be absolute"));
    }
    let mut blocked = vec![
        personal.home.join(".config/herdr"),
        personal.home.join(".local/bin/herdr"),
        PathBuf::from("/opt/homebrew/bin/herdr"),
    ];
    if let Some(xdg) = &personal.xdg {
        blocked.push(xdg.join("herdr"));
    }
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
        || blocked.iter().any(|private| overlaps(path, private))
    {
        return Err(invalid("home is not a dedicated managed herdr directory"));
    }
    for private in blocked {
        if let Some(real) = private.parent().and_then(|p| fs::canonicalize(p).ok())
            && private
                .file_name()
                .is_some_and(|name| overlaps(path, &real.join(name)))
        {
            return Err(invalid("home aliases a personal herdr path"));
        }
    }
    Ok(())
}

fn resolve(path: &Path, personal: &PersonalPaths) -> io::Result<PathBuf> {
    protect(path, personal)?;
    for ancestor in path.ancestors().filter(|part| part.parent().is_some()) {
        match fs::symlink_metadata(ancestor) {
            Ok(meta) if meta.file_type().is_symlink() => {
                if !matches!(ancestor.to_str(), Some("/tmp" | "/var" | "/etc")) {
                    return Err(invalid("symlink in managed herdr home"));
                }
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound && ancestor == path => {}
            Err(error) => return Err(error),
        }
    }
    let real = match fs::canonicalize(path) {
        Ok(real) => real,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let parent =
                fs::canonicalize(path.parent().ok_or_else(|| invalid("home has no parent"))?)?;
            parent.join(
                path.file_name()
                    .ok_or_else(|| invalid("home has no name"))?,
            )
        }
        Err(error) => return Err(error),
    };
    protect(&real, personal)?;
    Ok(real)
}

/// A personal boundary inside a managed home would leave the real personal config unguarded.
fn personal_boundary<'a>(
    personal: &PersonalPaths,
    homes: impl Iterator<Item = &'a Path>,
) -> io::Result<()> {
    let boundaries: Vec<_> = std::iter::once(&personal.home)
        .chain(personal.xdg.as_ref())
        .flat_map(|path| [Some(path.clone()), fs::canonicalize(path).ok()])
        .flatten()
        .collect();
    for home in homes {
        let homes = [Some(home.to_path_buf()), fs::canonicalize(home).ok()];
        if homes
            .iter()
            .flatten()
            .any(|home| boundaries.iter().any(|path| path.starts_with(home)))
        {
            return Err(invalid(
                "personal boundary points into a managed herdr home",
            ));
        }
    }
    Ok(())
}

fn copied(error: &io::Error) -> io::Error {
    io::Error::new(error.kind(), error.to_string())
}

impl ManagedHome {
    pub(crate) fn local(boot: &BootSessionHosts, personal: &PersonalPaths) -> Vec<Target> {
        Self::of(boot.local_node(), boot.config(), personal)
    }

    /// Only this node's dedicated endpoints; a user-default endpoint is never resolved or opened.
    pub(crate) fn of(
        node: Option<&str>,
        hosts: &SessionHostsConfig,
        personal: &PersonalPaths,
    ) -> Vec<Target> {
        let Some(node) = node else {
            return Vec::new();
        };
        let endpoints: Vec<_> = hosts
            .herdr
            .endpoints
            .iter()
            .filter(|(_, e)| e.mode == EndpointMode::LegacyDedicated)
            .filter(|(_, e)| e.execution_node.trim() == node)
            .collect();
        if endpoints.is_empty() {
            return Vec::new();
        }
        let uid = personal_boundary(
            personal,
            endpoints.iter().map(|(_, e)| e.herdr_home.as_path()),
        )
        .and_then(|()| Ok(fs::metadata(&personal.home)?.uid()));
        let mut targets = Vec::new();
        let mut homes: BTreeMap<PathBuf, (Vec<String>, (PathBuf, PathBuf), bool)> = BTreeMap::new();
        for (key, endpoint) in endpoints {
            let resolved = uid
                .as_ref()
                .map_err(copied)
                .and_then(|_| resolve(&endpoint.herdr_home, personal));
            let path = match resolved {
                Ok(path) => path,
                Err(error) => {
                    targets.push(Target {
                        keys: vec![key.clone()],
                        declared: endpoint.herdr_home.clone(),
                        home: Err(error),
                    });
                    continue;
                }
            };
            let declaration = (endpoint.socket_path.clone(), endpoint.herdr_home.clone());
            let entry = homes
                .entry(path)
                .or_insert_with(|| (Vec::new(), declaration.clone(), false));
            entry.0.push(key.clone());
            entry.2 |= entry.1 != declaration;
        }
        for (path, (keys, (_, declared), conflict)) in homes {
            let home = match (conflict, &uid) {
                (true, _) => Err(invalid(
                    "one BOOT home has conflicting socket or config paths",
                )),
                (false, Err(error)) => Err(copied(error)),
                (false, Ok(uid)) => {
                    let target = Self { path, uid: *uid };
                    match fs::symlink_metadata(&target.path) {
                        Ok(_) => target.verify_dir().map(|()| target),
                        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(target),
                        Err(error) => Err(error),
                    }
                }
            };
            targets.push(Target {
                keys,
                declared,
                home,
            });
        }
        targets
    }

    pub(super) fn verify_dir(&self) -> io::Result<()> {
        let meta = fs::symlink_metadata(&self.path)?;
        if !meta.is_dir() || meta.uid() != self.uid || meta.mode() & 0o022 != 0 {
            return Err(invalid(
                "managed home needs its user's private write ownership",
            ));
        }
        Ok(())
    }

    pub(super) fn prepare_dir(&self) -> io::Result<()> {
        if let Err(error) = fs::symlink_metadata(&self.path) {
            if error.kind() != io::ErrorKind::NotFound {
                return Err(error);
            }
            let mut builder = fs::DirBuilder::new();
            builder.mode(0o700);
            match builder.create(&self.path) {
                Ok(()) => fsync_parent_dir(&self.path)?,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        self.verify_dir()
    }
}
