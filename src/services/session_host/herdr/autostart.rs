//! Whether AgentDesk may start the execution user's own herdr for a user_default socket: a dry-run
//! `assess` for the probe, and `spawn::ensure` for an input that needs the server.

#[path = "autostart/candidate.rs"]
pub(crate) mod candidate;
#[path = "autostart/presence.rs"]
pub(crate) mod presence;
#[path = "autostart/record.rs"]
pub(crate) mod record;
#[path = "autostart/selector.rs"]
pub(crate) mod selector;
#[path = "autostart/spawn.rs"]
pub(crate) mod spawn;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::{Value, json};

/// What one assessment reads from this process: the execution user's HOME, its PATH, and the
/// binaries it must never start.
#[derive(Debug, Clone, Default)]
pub(crate) struct Environment {
    pub home: Option<PathBuf>,
    pub path: Option<OsString>,
    pub excluded: Vec<PathBuf>,
}

impl Environment {
    /// The execution user's `home` with this process's PATH; AgentDesk's own `~/.adk` copies are
    /// excluded.
    pub(crate) fn for_home(home: Option<PathBuf>) -> Self {
        let excluded = home.iter().map(|home| home.join(".adk")).collect();
        Self {
            home,
            path: std::env::var_os("PATH"),
            excluded,
        }
    }
}

/// One dry-run assessment as JSON; `eligible` never authorizes a start.
pub(crate) async fn assess(socket: &Path, env: Environment) -> Value {
    let owned = socket.to_path_buf();
    let presence =
        tokio::task::spawn_blocking(move || presence::classify(&owned, &presence::HostOs))
            .await
            .unwrap_or(presence::Presence::Unknown(
                presence::Unknown::ProcessTable("presence reader panicked".into()),
            ));
    let mut value = json!({
        "presence": presence::describe(&presence),
        "observation": "dry_run; nothing was started, locked or written, and eligibility grants nothing",
    });
    let verdict = match presence {
        presence::Presence::Present => Err("not_needed_present"),
        presence::Presence::Unknown(_) => Err("presence_unknown"),
        presence::Presence::Absent(_) if !cfg!(target_os = "macos") => Err("platform_unsupported"),
        presence::Presence::Absent(_) => match target(socket, &env) {
            Ok(home) => prepare(socket, home, &env, &mut value).await.map(drop),
            Err(reason) => Err(reason),
        },
    };
    let reason = verdict.err().unwrap_or("eligible");
    value["eligible"] = (reason == "eligible").into();
    value["reason"] = reason.into();
    value["action"] = action(reason).map_or(Value::Null, Value::from);
    value
}

/// Why a start is refused, as the stable word health and the probe show.
pub(crate) type Refusal = &'static str;

/// The checks that need no IO: HOME and that `socket` is its default socket.
pub(crate) fn target<'a>(socket: &Path, env: &'a Environment) -> Result<&'a Path, Refusal> {
    let home = env
        .home
        .as_deref()
        .filter(|home| home.is_absolute())
        .ok_or("home_unavailable")?;
    if socket != home.join(".config/herdr/herdr.sock") {
        return Err("socket_not_default");
    }
    Ok(home)
}

/// The socket dir, the config and the binary a start of `home`'s default server would use.
pub(crate) async fn prepare(
    socket: &Path,
    home: &Path,
    env: &Environment,
    value: &mut Value,
) -> Result<candidate::Candidate, Refusal> {
    let dir = home.join(".config/herdr");
    let euid = presence::PresenceOs::euid(&presence::HostOs);
    if !owned_private_dir(&dir, euid) {
        return Err("socket_dir_unsafe");
    }
    let config = dir.join("config.toml");
    match super::policy::file::read(&config) {
        Err(_) if !config.exists() => return Err("config_resume_not_off"),
        Err(_) => return Err("config_unreadable"),
        Ok(file) if super::policy::explicit_false(&file.bytes).is_err() => {
            return Err("config_resume_not_off");
        }
        Ok(_) => {}
    }
    let dirs = candidate::search_dirs(home, env.path.as_deref());
    let excluded = env.excluded.clone();
    let (found, rejected) =
        tokio::task::spawn_blocking(move || candidate::find(&dirs, &excluded, euid))
            .await
            .unwrap_or_default();
    value["rejected"] = rejected
        .iter()
        .map(|r| json!({"path": r.path.display().to_string(), "why": r.why}))
        .collect();
    let found = found.ok_or("binary_not_found")?;
    value["binary"] = found.path.display().to_string().into();
    value["version"] = found.version.clone().into();
    let checker = super::policy::checker::Checker {
        binary: &found.path,
        home,
        config: &config,
        socket,
    };
    checker
        .candidate()
        .await
        .map_err(|_| "config_check_failed")?;
    Ok(found)
}

/// One line of what to do for each refusal; `None` where nothing needs doing.
pub(crate) fn action(reason: &str) -> Option<&'static str> {
    Some(match reason {
        "eligible" | "not_needed_present" | "started" => return None,
        "config_resume_not_off" => {
            "The execution user's herdr config does not set `session.resume_agents_on_restore = false`, so AgentDesk will not start a server (a restore could resume previous agents). Without a running server no consent card can be offered. Set it to false in that config (the next input or `!herdr consent retry` then starts the server), or start `herdr` yourself so the consent card can be offered."
        }
        "presence_unknown" => {
            "Whether a server is running could not be ruled out (an unclear socket, or a herdr process that selects it); see `presence`. AgentDesk starts nothing until that clears."
        }
        "socket_not_default" => {
            "AgentDesk starts only the default `~/.config/herdr/herdr.sock` server; start a named or custom-socket server yourself."
        }
        "socket_dir_unsafe" => {
            "Make ~/.config/herdr a real directory owned by the execution user and not group- or world-writable."
        }
        "config_unreadable" => "Make ~/.config/herdr/config.toml a readable regular file.",
        "binary_not_found" => {
            "Install a native arm64 herdr 0.9.3 or newer owned by the execution user, e.g. with Homebrew; `rejected` lists what was refused."
        }
        "config_check_failed" => {
            "Run `herdr config check` as the execution user and fix what it reports."
        }
        "backoff" => {
            "AgentDesk started this server three times within ten minutes and stops trying for now; see the server log beside the start record."
        }
        "lock_timeout" => "Another start of this server held its lock; the next input retries.",
        "not_ready" => {
            "The started server did not answer within ten seconds; it was left running, and the next input checks it again."
        }
        "remote" => "Only the execution node's AgentDesk starts its user's server.",
        "platform_unsupported" => "Automatic start is macOS-only; start herdr yourself.",
        _ => "Start `herdr` as the execution user yourself.",
    })
}

fn owned_private_dir(dir: &Path, euid: u32) -> bool {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(dir)
        .is_ok_and(|meta| meta.is_dir() && meta.uid() == euid && meta.mode() & 0o022 == 0)
}

static LAST: Mutex<BTreeMap<PathBuf, Value>> = Mutex::new(BTreeMap::new());

/// The last start decision for `socket` in this process, for health.
pub(crate) fn last(socket: &Path) -> Option<Value> {
    LAST.lock().ok()?.get(socket).cloned()
}

fn remember(socket: &Path, value: &Value) {
    if let Ok(mut last) = LAST.lock() {
        last.insert(socket.into(), value.clone());
    }
}

#[cfg(all(test, target_os = "macos"))]
#[path = "autostart_tests.rs"]
mod tests;

#[cfg(all(test, unix))]
#[path = "autostart_real_tests.rs"]
pub(crate) mod real_tests;
