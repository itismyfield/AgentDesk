use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Component, Path, PathBuf};

use super::invalid;
use crate::config::session_hosts::BootSessionHosts;
use crate::services::discord::runtime_store::fsync_parent_dir;

#[derive(Debug)]
pub(crate) struct PersonalPaths {
    pub home: PathBuf,
    pub xdg: Option<PathBuf>,
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

impl ManagedHome {
    pub(crate) fn local(
        boot: &BootSessionHosts,
        personal: &PersonalPaths,
    ) -> io::Result<Vec<Self>> {
        let Some(node) = boot.local_node() else {
            return Ok(Vec::new());
        };
        let mut homes = BTreeMap::new();
        for endpoint in boot
            .config()
            .herdr
            .endpoints
            .values()
            .filter(|e| e.execution_node.trim() == node)
        {
            let path = resolve(&endpoint.herdr_home, personal)?;
            let declaration = (endpoint.socket_path.clone(), endpoint.herdr_home.clone());
            if let Some(previous) = homes.insert(path, declaration.clone())
                && previous != declaration
            {
                return Err(invalid(
                    "one BOOT home has conflicting socket or config paths",
                ));
            }
        }
        if homes.is_empty() {
            return Ok(Vec::new());
        }
        let uid = fs::metadata(&personal.home)?.uid();
        let targets: Vec<_> = homes.into_keys().map(|path| Self { path, uid }).collect();
        for target in &targets {
            match fs::symlink_metadata(&target.path) {
                Ok(_) => target.verify_dir()?,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok(targets)
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
