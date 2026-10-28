use super::{Flight, Op, before};
use std::collections::HashMap;
#[cfg(unix)]
use std::path::Path;
use std::process::Command;
use std::sync::{Mutex, OnceLock, PoisonError};

fn buffers() -> &'static Mutex<HashMap<String, String>> {
    static BUFFERS: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    BUFFERS.get_or_init(Default::default)
}

pub(super) fn clear_buffers() {
    buffers()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clear();
}

pub(crate) struct BufferLoad {
    name: String,
    text: String,
    input_complete: bool,
    active: bool,
    events: Vec<(super::Shared, usize)>,
}

pub(crate) fn begin_load_buffer(name: &str, text: &str) -> BufferLoad {
    let entries = super::registry()
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    buffers()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(name);
    let mut events = Vec::new();
    for entry in entries.iter() {
        let mut state = entry.lock().unwrap_or_else(PoisonError::into_inner);
        let scope = state
            .scopes
            .get(&std::thread::current().id())
            .and_then(|v| v.last())
            .cloned();
        if scope.is_some() {
            let index = state.counts.events.len();
            state.counts.events.push(super::Event {
                op: Op::Load,
                scope,
                accepted: true,
                written: false,
                bytes: text.len(),
                digest: super::digest(text.as_bytes()),
            });
            events.push((entry.clone(), index));
        }
    }
    BufferLoad {
        name: name.into(),
        text: text.into(),
        input_complete: false,
        active: !entries.is_empty(),
        events,
    }
}

impl BufferLoad {
    pub(crate) fn input_complete(&mut self) {
        self.input_complete = true;
    }

    pub(crate) fn succeeded(&mut self) {
        if !self.input_complete || !self.active {
            return;
        }
        buffers()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(self.name.clone(), self.text.clone());
        for (state, index) in &self.events {
            state
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .counts
                .events[*index]
                .written = true;
        }
    }
}

pub(super) fn buffer_text(name: &str) -> Option<String> {
    buffers()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(name)
        .cloned()
}

fn command_index(args: &[String]) -> usize {
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        if arg == "--" {
            return index + 1;
        }
        if !arg.starts_with('-') {
            break;
        }
        index += if matches!(arg.as_str(), "-L" | "-S" | "-f" | "-T" | "-c") {
            2
        } else {
            1
        };
    }
    index
}

pub(super) fn verb(args: &[String]) -> &str {
    args.get(command_index(args)).map_or("", String::as_str)
}

fn pane_identity(command: &Command, args: &[String], target: Option<&str>) -> Option<String> {
    let mut query = Command::new(command.get_program());
    query.args(&args[..command_index(args).min(args.len())]);
    for (key, value) in command.get_envs() {
        if let Some(value) = value {
            query.env(key, value);
        } else {
            query.env_remove(key);
        }
    }
    if let Some(cwd) = command.get_current_dir() {
        query.current_dir(cwd);
    }
    query.args(["display-message", "-p"]);
    if let Some(target) = target {
        query.args(["-t", target]);
    }
    query.arg("#{socket_path}\t#{pane_id}");
    // Reuse the existing bounded child lifecycle; this read-only verb cannot recurse into resolution.
    let output = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .ok()?
            .block_on(crate::services::tui_input::bounded_tmux::run_bounded_tmux(
                &mut query,
            ))
            .ok()
    })
    .join()
    .ok()??;
    if !output.status.success() {
        return None;
    }
    let identity = String::from_utf8(output.stdout).ok()?;
    let identity = identity.trim_end();
    let (socket, pane) = identity.split_once('\t')?;
    (!socket.is_empty()
        && pane
            .strip_prefix('%')
            .is_some_and(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit())))
    .then(|| identity.into())
}

fn normalize_target(
    command: &Command,
    args: &[String],
    raw: Option<&str>,
    op: Op,
) -> Result<String, String> {
    let physical = raw
        .unwrap_or("")
        .trim_start_matches('=')
        .trim_end_matches(':');
    let protected: Vec<_> = super::registry()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .filter_map(|entry| {
            let state = entry.lock().unwrap_or_else(PoisonError::into_inner);
            if state.target.host == "tmux" {
                Some(state.target.physical.clone())
            } else {
                None
            }
        })
        .collect();
    if !op.guarded()
        || op == Op::Create
        || protected.is_empty()
        || protected.iter().any(|target| target == physical)
    {
        return Ok(physical.into());
    }
    let identity = pane_identity(command, args, raw);
    for target in protected {
        let protected_identity = pane_identity(command, args, Some(&target));
        if identity.is_some() && protected_identity == identity {
            return Ok(target);
        }
        if identity.is_none() && protected_identity.is_some() {
            let _ = before("tmux", &target, Op::Unclassified, &[]);
        }
    }
    if identity.is_none() {
        return Err("B0 refused a write whose physical tmux target could not be resolved".into());
    }
    Ok(physical.into())
}

pub(crate) fn tmux(command: &Command) -> Result<Flight, String> {
    let args: Vec<_> = command
        .get_args()
        .map(|v| v.to_string_lossy().into_owned())
        .collect();
    let flag = |name: &str| {
        let mut flags = args[command_index(&args).min(args.len())..].iter().skip(1);
        while let Some(arg) = flags.next() {
            if arg == "--" {
                break;
            }
            if arg == name {
                return flags.next().map(String::as_str);
            }
            if let Some(value) = arg.strip_prefix(name).filter(|value| !value.is_empty()) {
                return Some(value);
            }
        }
        None
    };
    let physical = flag("-t")
        .or_else(|| flag("-s"))
        .unwrap_or("")
        .trim_start_matches('=')
        .trim_end_matches(':');
    let verb = verb(&args);
    let (op, payload) = if verb == "paste-buffer" {
        let payload = flag("-b")
            .and_then(|name| {
                buffers()
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .get(name)
                    .cloned()
            })
            .unwrap_or_default();
        (Op::Paste, payload)
    } else if verb == "send-keys" {
        if args.iter().any(|v| v == "-l") {
            (Op::Literal, args.last().cloned().unwrap_or_default())
        } else {
            let start = args
                .iter()
                .enumerate()
                .skip(command_index(&args) + 1)
                .find(|(_, arg)| arg.starts_with("-t"))
                .map_or(command_index(&args) + 1, |(i, arg)| {
                    i + if arg == "-t" { 2 } else { 1 }
                })
                .min(args.len());
            let keys = &args[start..];
            let control = |key: &str| {
                matches!(
                    key,
                    "Enter"
                        | "Escape"
                        | "Left"
                        | "Right"
                        | "Up"
                        | "Down"
                        | "Home"
                        | "End"
                        | "BSpace"
                        | "Tab"
                        | "Space"
                ) || key.starts_with("C-")
            };
            let payload = keys
                .iter()
                .filter(|key| !control(key))
                .cloned()
                .collect::<Vec<_>>()
                .join("");
            let enter = keys.iter().any(|key| key == "Enter");
            let op = if !payload.is_empty() {
                if enter { Op::Line } else { Op::Literal }
            } else if enter {
                Op::Enter
            } else {
                Op::Control
            };
            (op, payload)
        }
    } else if verb == "new-session" {
        let shell = args.last().cloned().unwrap_or_default();
        let entries = super::registry()
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let prompt = entries.iter().find_map(|entry| {
            let state = entry.lock().unwrap_or_else(PoisonError::into_inner);
            (state.target.host == "tmux" && state.target.physical == physical)
                .then(|| state.argv.get(&shell).cloned())
                .flatten()
        });
        match prompt {
            Some(prompt) => (
                Op::Argv,
                String::from_utf8(prompt).map_err(|e| e.to_string())?,
            ),
            None => (Op::Create, shell),
        }
    } else if verb == "if-shell" {
        (Op::Unclassified, String::new())
    } else if verb == "capture-pane" {
        (Op::Capture, String::new())
    } else if matches!(
        verb,
        "display-message"
            | "has-session"
            | "kill-session"
            | "load-buffer"
            | "delete-buffer"
            | "set-option"
            | "set-window-option"
            | "set-hook"
            | "show-options"
            | "list-sessions"
    ) {
        (Op::Control, String::new())
    } else {
        (Op::Unclassified, String::new())
    };
    let physical = normalize_target(command, &args, flag("-t").or_else(|| flag("-s")), op)?;
    before("tmux", &physical, op, payload.as_bytes())
}

#[cfg(unix)]
pub(crate) fn herdr(socket: &Path, call: &impl serde::Serialize) -> Result<Flight, String> {
    let call = serde_json::to_value(call).map_err(|error| error.to_string())?;
    let pane = call["params"]["pane_id"].as_str().unwrap_or("");
    let text = call["params"]["text"].as_str().unwrap_or("");
    let (op, payload) = match call["method"].as_str() {
        Some("pane.send_text") => {
            match text
                .strip_prefix("\u{1b}[200~")
                .and_then(|text| text.strip_suffix("\u{1b}[201~"))
            {
                Some(body) => (Op::Paste, body),
                None => (Op::Text, text),
            }
        }
        Some("pane.send_input") => (Op::Line, text),
        Some("pane.send_keys") => {
            let enter = call["params"]["keys"]
                .as_array()
                .is_some_and(|keys| keys.iter().any(|key| key == "enter"));
            (if enter { Op::Enter } else { Op::Control }, "")
        }
        Some("pane.read") => (Op::Capture, ""),
        _ => return Ok(Flight::default()),
    };
    before(
        "herdr",
        &format!("{}#{pane}", socket.display()),
        op,
        payload.as_bytes(),
    )
}

#[test]
fn s5_b0_transport_classifies_text_with_enter_as_one_payload_and_control_as_zero() {
    let (_root, attempt, guard) = super::tests::fixture("tmux", &uuid::Uuid::new_v4().to_string());
    let _scope = guard.scope(super::Scope::owner(attempt.clone()));
    for key in ["C-c", "C-s", "Escape", "Left", "C-u"] {
        let mut command = Command::new("fixture-tmux");
        command.args(["send-keys", "-t", &attempt.target.physical, key]);
        tmux(&command).unwrap().written();
    }
    let mut command = Command::new("fixture-tmux");
    command.args([
        "send-keys",
        "-t",
        &attempt.target.physical,
        "payload",
        "Enter",
    ]);
    tmux(&command).unwrap().written();
    guard.assert_owner_content(&attempt, 1);
    let counts = guard.counts();
    assert_eq!(
        counts
            .events
            .iter()
            .filter(|event| event.op == Op::Control)
            .count(),
        5
    );
    assert_eq!(counts.events.last().unwrap().op, Op::Line);
}

#[test]
fn s5_b0_payload_words_cannot_select_the_tmux_command_verb() {
    let (_root, attempt, guard) = super::tests::fixture("tmux", &uuid::Uuid::new_v4().to_string());
    let _scope = guard.scope(super::Scope::owner(attempt.clone()));
    for payload in ["paste-buffer", "if-shell", "new-session"] {
        let mut command = Command::new("fixture-tmux");
        command.args([
            "-u",
            "send-keys",
            "-t",
            &attempt.target.physical,
            "-l",
            "--",
            payload,
        ]);
        tmux(&command).unwrap().written();
    }
    guard.assert_owner_content(&attempt, 3);
    assert!(
        guard
            .counts()
            .events
            .iter()
            .all(|event| event.op == Op::Literal)
    );
}
