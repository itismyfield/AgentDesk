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

pub(crate) fn load_buffer(name: &str, text: &str) {
    let entries = super::registry()
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if entries.is_empty() {
        return;
    }
    buffers()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(name.into(), text.into());
    for entry in entries.iter() {
        let mut state = entry.lock().unwrap_or_else(PoisonError::into_inner);
        let scope = state
            .scopes
            .get(&std::thread::current().id())
            .and_then(|v| v.last())
            .cloned();
        if scope.is_some() {
            state.counts.events.push(super::Event {
                op: Op::Load,
                scope,
                accepted: true,
                written: false,
                bytes: text.len(),
                digest: super::digest(text.as_bytes()),
            });
        }
    }
}

pub(crate) fn tmux(command: &Command) -> Result<Flight, String> {
    let args: Vec<_> = command
        .get_args()
        .map(|v| v.to_string_lossy().into_owned())
        .collect();
    let flag = |name| {
        args.windows(2)
            .find(|v| v[0] == name)
            .map(|v| v[1].as_str())
    };
    let physical = flag("-t")
        .or_else(|| flag("-s"))
        .unwrap_or("")
        .trim_start_matches('=')
        .trim_end_matches(':');
    let (op, payload) = if args.iter().any(|v| v == "paste-buffer") {
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
    } else if args.iter().any(|v| v == "send-keys") {
        if args.iter().any(|v| v == "-l") {
            (Op::Literal, args.last().cloned().unwrap_or_default())
        } else {
            let start = args
                .iter()
                .position(|v| v == "-t")
                .map_or(args.len(), |i| i + 2);
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
    } else if args.iter().any(|v| v == "new-session") {
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
    } else if args.iter().any(|v| v == "capture-pane") {
        (Op::Capture, String::new())
    } else {
        (Op::Control, String::new())
    };
    before("tmux", physical, op, payload.as_bytes())
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
