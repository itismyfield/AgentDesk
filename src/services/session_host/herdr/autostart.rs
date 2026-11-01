//! Read-only judgement of whether AgentDesk could start the user's own herdr for a user_default
//! socket; nothing here locks, spawns, writes or grants, and a start re-judges it under its lock.

#[path = "autostart/candidate.rs"]
pub(crate) mod candidate;
#[path = "autostart/presence.rs"]
pub(crate) mod presence;
#[path = "autostart/selector.rs"]
pub(crate) mod selector;

use std::ffi::OsString;
use std::path::{Path, PathBuf};

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
        "eligible": false,
        "observation": "dry_run; nothing was started, locked or written, and eligibility grants nothing",
    });
    let verdict = match presence {
        presence::Presence::Present => Err(("not_needed_present", None)),
        presence::Presence::Unknown(_) => Err(("presence_unknown", None)),
        presence::Presence::Absent(_) => gates(socket, &env, &mut value).await,
    };
    let (reason, action) = match verdict {
        Ok(()) => ("eligible", None),
        Err(refusal) => refusal,
    };
    value["eligible"] = (reason == "eligible").into();
    value["reason"] = reason.into();
    value["action"] = action.map_or(Value::Null, Value::from);
    value
}

type Refusal = (&'static str, Option<&'static str>);

/// The checks an absent default socket must pass before a start could be considered.
async fn gates(socket: &Path, env: &Environment, value: &mut Value) -> Result<(), Refusal> {
    if !cfg!(target_os = "macos") {
        return Err(("platform_unsupported", None));
    }
    let home = env
        .home
        .as_deref()
        .filter(|home| home.is_absolute())
        .ok_or(("home_unavailable", None))?;
    let dir = home.join(".config/herdr");
    if socket != dir.join("herdr.sock") {
        return Err(("socket_not_default", None));
    }
    let euid = presence::PresenceOs::euid(&presence::HostOs);
    if !owned_private_dir(&dir, euid) {
        return Err((
            "socket_dir_unsafe",
            Some(
                "Make ~/.config/herdr a real directory owned by the execution user and not group- or world-writable.",
            ),
        ));
    }
    let config = dir.join("config.toml");
    let resume = "Set `[session] resume_agents_on_restore = false` in ~/.config/herdr/config.toml; AgentDesk only considers a server that cannot resume agents.";
    match super::policy::file::read(&config) {
        Err(_) if !config.exists() => return Err(("config_resume_not_off", Some(resume))),
        Err(_) => return Err(("config_unreadable", None)),
        Ok(file) if super::policy::explicit_false(&file.bytes).is_err() => {
            return Err(("config_resume_not_off", Some(resume)));
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
    let found = found.ok_or((
        "binary_not_found",
        Some("Install a native arm64 herdr 0.9.3 or newer owned by the execution user, e.g. with Homebrew."),
    ))?;
    value["binary"] = found.path.display().to_string().into();
    value["version"] = found.version.into();
    let checker = super::policy::checker::Checker {
        binary: &found.path,
        home,
        config: &config,
        socket,
    };
    checker.candidate().await.map_err(|_| {
        (
            "config_check_failed",
            Some("Run `herdr config check` as the execution user and fix what it reports."),
        )
    })
}

fn owned_private_dir(dir: &Path, euid: u32) -> bool {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(dir)
        .is_ok_and(|meta| meta.is_dir() && meta.uid() == euid && meta.mode() & 0o022 == 0)
}

#[cfg(all(test, unix))]
#[path = "autostart_real_tests.rs"]
mod real_tests;
