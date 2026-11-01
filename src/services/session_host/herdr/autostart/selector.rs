//! The socket a herdr process serves or would start, resolved as herdr 0.9.5 does: explicit session
//! (last wins) before `HERDR_SOCKET_PATH`, before `HERDR_SESSION`, before the default.

use std::path::{Path, PathBuf};

/// What the process does with its selected socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    /// `herdr server`: binds the socket.
    Server,
    /// The TUI client or `session attach`: starts a server on the socket when none answers.
    Launcher,
    /// Any other command; it never binds a local server.
    Other,
    /// An option this reader does not know; the role cannot be ruled out.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Selection {
    pub role: Role,
    /// `None` when the environment cannot resolve one socket.
    pub socket: Option<PathBuf>,
}

/// Options that take a value and are not a session.
const VALUED: [&str; 3] = ["--remote", "--machine", "--remote-keybindings"];
/// Options after which herdr prints and exits.
const EXITING: [&str; 6] = [
    "--version",
    "-V",
    "--help",
    "-h",
    "--default-config",
    "--skill",
];

pub(crate) fn select(argv: &[String], environ: &[String]) -> Selection {
    let mut session = None;
    let mut positional = Vec::new();
    let (mut remote, mut exits, mut unknown) = (false, false, false);
    let mut args = argv.iter().skip(1);
    while let Some(arg) = args.next() {
        let arg = arg.as_str();
        if arg == "--" {
            positional.extend(args.by_ref().map(String::as_str));
        } else if arg == "--session" {
            session = args.next().map(String::as_str).or(Some(""));
        } else if let Some(name) = arg.strip_prefix("--session=") {
            session = Some(name);
        } else if VALUED.contains(&arg) {
            remote |= arg != "--remote-keybindings";
            args.next();
        } else if EXITING.contains(&arg) {
            exits = true;
        } else if arg.starts_with('-') && arg != "--handoff" {
            unknown = true;
        } else {
            positional.push(arg);
        }
    }
    let role = match positional.as_slice() {
        _ if remote || exits => Role::Other,
        _ if unknown => Role::Unknown,
        [] => Role::Launcher,
        ["server"] => Role::Server,
        ["session", "attach", name] => {
            session = Some(*name);
            Role::Launcher
        }
        _ => Role::Other,
    };
    Selection {
        role,
        socket: socket(session, environ),
    }
}

/// The one value of `key`, `Some(None)` when unset, `None` when set more than once.
fn single<'a>(environ: &'a [String], key: &str) -> Option<Option<&'a str>> {
    let prefix = format!("{key}=");
    let mut values = environ
        .iter()
        .filter_map(|e| e.strip_prefix(prefix.as_str()));
    let first = values.next();
    values.next().is_none().then_some(first)
}

fn absolute(value: &str) -> Option<PathBuf> {
    Path::new(value).is_absolute().then(|| value.into())
}

fn socket(explicit: Option<&str>, environ: &[String]) -> Option<PathBuf> {
    let dir = match single(environ, "XDG_CONFIG_HOME")? {
        Some(xdg) => absolute(xdg)?.join("herdr"),
        None => absolute(single(environ, "HOME")??)?.join(".config/herdr"),
    };
    let named = |name: &str| match name {
        "" => None,
        "default" => Some(dir.join("herdr.sock")),
        name => Some(dir.join("sessions").join(name).join("herdr.sock")),
    };
    if let Some(name) = explicit {
        return named(name);
    }
    if let Some(path) = single(environ, "HERDR_SOCKET_PATH")? {
        return absolute(path);
    }
    match single(environ, "HERDR_SESSION")? {
        Some(name) => named(name),
        None => named("default"),
    }
}

#[cfg(test)]
#[path = "selector_tests.rs"]
mod tests;
