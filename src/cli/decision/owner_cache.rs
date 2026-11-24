//! The requester's capabilities on this host: one 256-bit secret per asked request, kept in a
//! private directory under the runtime root. A capability is never printed.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::services::discord::decisions::requester::well_formed;

/// What one capability belongs to: the asking agent, its target channel and the ask's key.
pub(super) struct Scope {
    agent: String,
    channel: String,
    key: String,
}

impl Scope {
    /// The scope an ask body names; its idempotency key is the ref when none is given.
    pub(super) fn of(body: &Value) -> Result<Self, String> {
        fn text(value: &Value) -> Option<&str> {
            value
                .as_str()
                .map(str::trim)
                .filter(|text| !text.is_empty())
        }
        fn field(name: &str, value: Option<&str>) -> Result<String, String> {
            let missing = || format!("decision ask: the body needs {name}");
            value.map(str::to_string).ok_or_else(missing)
        }
        let key = text(&body["idempotency_key"]).or_else(|| text(&body["ref"]));
        Ok(Self {
            agent: field("agent_id", text(&body["agent_id"]))?,
            channel: field("target.channel_id", text(&body["target"]["channel_id"]))?,
            key: field("ref", key)?,
        })
    }

    /// The file of this scope: hashed names, so no part of the body becomes a path.
    fn path(&self, root: &Path) -> PathBuf {
        let name = |parts: &[&str]| hex::encode(Sha256::digest(parts.join("\0").as_bytes()));
        root.join("decision-owner")
            .join(name(&[self.agent.as_str(), self.channel.as_str()]))
            .join(format!("{}.cap", name(&[self.key.as_str()])))
    }
}

/// The capability of `scope`: the cached one when this ask was prepared before, otherwise a new
/// one that is on disk, readable by this user only, before it is returned.
pub(super) fn prepare(root: &Path, scope: &Scope) -> Result<String, String> {
    let fresh = hex::encode(rand::random::<[u8; 32]>());
    install(&scope.path(root), &fresh)
}

fn by_request(root: &Path, request_id: uuid::Uuid) -> PathBuf {
    let name = format!("{request_id}.cap");
    root.join("decision-owner").join("requests").join(name)
}

/// Links the request the server answered to the capability it was asked under.
pub(super) fn remember(
    root: &Path,
    request_id: uuid::Uuid,
    capability: &str,
) -> Result<(), String> {
    install(&by_request(root, request_id), capability).map(drop)
}

/// The capability this host asked `request_id` under; without one the request is not ours.
pub(super) fn of_request(root: &Path, request_id: uuid::Uuid) -> Result<String, String> {
    let unknown = |_| "decision get: this host did not ask that request".to_string();
    let saved = std::fs::read_to_string(by_request(root, request_id)).map_err(unknown)?;
    checked(saved.trim())
}

fn checked(saved: &str) -> Result<String, String> {
    match well_formed(saved) {
        true => Ok(saved.to_string()),
        false => Err("decision owner cache: the cached file is not a capability".into()),
    }
}

/// Puts `capability` at `path` in a private directory unless a file is already there, and
/// answers the capability the file holds.
fn install(path: &Path, capability: &str) -> Result<String, String> {
    let failed =
        |what: &str, error: std::io::Error| format!("decision owner cache: {what}: {error}");
    let dir = path.parent().ok_or("decision owner cache: no directory")?;
    std::fs::create_dir_all(dir).map_err(|error| failed("create", error))?;
    for private_dir in [dir.parent().unwrap_or(dir), dir] {
        private(private_dir, 0o700).map_err(|error| failed("protect", error))?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    match options.open(path) {
        Ok(mut file) => {
            let written = file
                .write_all(capability.as_bytes())
                .and_then(|()| file.sync_all());
            written.map_err(|error| failed("write", error))?;
            Ok(capability.to_string())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let saved = std::fs::read_to_string(path).map_err(|error| failed("read", error))?;
            checked(saved.trim())
        }
        Err(error) => Err(failed("create", error)),
    }
}

/// Restricts `path` to this user where the platform has modes.
fn private(path: &Path, mode: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
    Ok(())
}
