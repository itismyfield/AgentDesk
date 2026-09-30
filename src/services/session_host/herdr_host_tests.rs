use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;
use std::sync::Mutex;

use serde_json::{Value, json};

use super::*;
use crate::services::session_host::herdr::contract::HerdrTransportError;
use crate::services::session_host::herdr::model::{
    ExecutionState, HerdrReadSource, HerdrReply, HerdrResult, PaneState,
};

const PANE: &str = "w1-1";

// Replies are schema-shaped (protocol 22, schema_version 1) JSON bodies.
enum Scripted {
    Result(Value),
    Error(&'static str),
    WrongId,
    Fail(HerdrTransportError),
}

/// The request a step expects and the reply it gives; any other request fails.
type Step = (HerdrRequest, Scripted);

#[derive(Default)]
struct FakeTransport {
    script: Mutex<VecDeque<Step>>,
    calls: Mutex<Vec<HerdrCall>>,
    /// Moves to a new connection on every call, like a reconnecting transport.
    reconnects: bool,
    generation: AtomicU64,
}

impl HerdrTransport for FakeTransport {
    fn call(&self, call: &HerdrCall) -> (contract::HerdrOutcome, u64) {
        self.calls.lock().unwrap().push(call.clone());
        let generation = if self.reconnects {
            self.generation.fetch_add(1, Ordering::SeqCst) + 1
        } else {
            self.generation.load(Ordering::SeqCst)
        };
        (self.reply(call), generation)
    }
}

impl FakeTransport {
    fn reply(&self, call: &HerdrCall) -> contract::HerdrOutcome {
        let next = self.script.lock().unwrap().pop_front();
        let (expected, reply) = next.expect("fake transport called more often than scripted");
        assert_eq!(call.request, expected, "fake transport request mismatch");
        let body = match reply {
            Scripted::Fail(error) => return Err(error),
            Scripted::Result(result) => json!({"id": call.id, "result": result}),
            Scripted::WrongId => json!({"id": "other", "result": {"type": "ok"}}),
            Scripted::Error(code) => {
                json!({"id": call.id, "error": {"code": code, "message": "boom"}})
            }
        };
        Ok(serde_json::from_value::<HerdrReply>(body).expect("fixture matches the schema"))
    }
}

fn fake(script: Vec<Step>, reconnects: bool) -> HerdrHost<FakeTransport> {
    let endpoint = HerdrEndpoint::new("mac-mini", "pilot", Path::new("/tmp/h.sock"), "adk")
        .expect("valid endpoint");
    let transport = FakeTransport {
        script: Mutex::new(script.into()),
        reconnects,
        ..FakeTransport::default()
    };
    HerdrHost::new(endpoint, transport)
}

fn host(script: Vec<Step>) -> HerdrHost<FakeTransport> {
    fake(script, false)
}

fn calls(host: &HerdrHost<FakeTransport>) -> Vec<HerdrCall> {
    host.transport.calls.lock().unwrap().clone()
}

fn snapshot_call() -> HerdrRequest {
    HerdrRequest::SessionSnapshot {}
}

fn process_call() -> HerdrRequest {
    HerdrRequest::PaneProcessInfo {
        pane_id: PANE.into(),
    }
}

fn send_call(text: &str) -> HerdrRequest {
    HerdrRequest::PaneSendText {
        pane_id: PANE.into(),
        text: text.into(),
    }
}

fn pane_info(pane_id: &str) -> Value {
    json!({
        "pane_id": pane_id, "terminal_id": "t1", "workspace_id": "w1", "tab_id": "w1:1",
        "focused": false, "agent_status": "idle", "revision": 7,
        "cwd": "/work", "foreground_cwd": "/work/sub"
    })
}

fn snapshot(protocol: u32, panes: &[&str]) -> Scripted {
    let panes: Vec<Value> = panes.iter().map(|pane| pane_info(pane)).collect();
    Scripted::Result(json!({"type": "session_snapshot", "snapshot": {
        "version": "0.9.x", "protocol": protocol, "workspaces": [], "tabs": [],
        "panes": panes, "layouts": [], "agents": []
    }}))
}

fn process_info(pane_id: &str, shell_pid: Value) -> Scripted {
    Scripted::Result(json!({"type": "pane_process_info", "process_info": {
        "pane_id": pane_id, "shell_pid": shell_pid, "foreground_process_group_id": 5151,
        "foreground_processes": [{"pid": 6161, "name": "claude"}], "tty": "/dev/ttys001"
    }}))
}

fn read(source: &str, truncated: bool) -> Scripted {
    Scripted::Result(json!({"type": "pane_read", "read": {
        "pane_id": PANE, "workspace_id": "w1", "tab_id": "w1:1", "source": source,
        "format": "text", "text": "screen", "revision": 3, "truncated": truncated
    }}))
}

fn ok() -> Scripted {
    Scripted::Result(json!({"type": "ok"}))
}

fn not_sent() -> Scripted {
    Scripted::Fail(HerdrTransportError::NotSent("connect refused".into()))
}

fn after_write() -> Scripted {
    Scripted::Fail(HerdrTransportError::AfterWrite("eof after write".into()))
}

fn pane() -> HostSessionRef<'static> {
    HostSessionRef::herdr_pane(PANE)
}

#[test]
fn herdr_complete_snapshot_decides_present_or_missing() {
    let present = host(vec![(snapshot_call(), snapshot(22, &["w1-0", PANE]))]);
    assert_eq!(present.presence(pane()), HostPresence::Present);
    let missing = host(vec![
        (snapshot_call(), snapshot(22, &["w1-0"])),
        (snapshot_call(), snapshot(22, &["w1-0"])),
    ]);
    assert_eq!(missing.presence(pane()), HostPresence::Missing);
    assert_eq!(missing.liveness(pane()), HostLiveness::DeadOrAbsent);
}

#[test]
fn herdr_probe_failures_never_read_as_missing_or_dead() {
    let failures: Vec<fn() -> Scripted> = vec![
        not_sent,
        after_write,
        || Scripted::Error("pane_not_found"),
        || Scripted::WrongId,
        || snapshot(21, &[]),
        ok,
    ];
    for failure in failures {
        let probe = host(vec![(snapshot_call(), failure())]);
        assert_eq!(
            probe.presence(pane()),
            HostPresence::ProbeFailed,
            "ProbeFailed must never read as Missing"
        );
        let probe = host(vec![(snapshot_call(), failure())]);
        assert_eq!(
            probe.liveness(pane()),
            HostLiveness::ProbeError,
            "a failed probe must never read as dead"
        );
    }
}

#[test]
fn herdr_liveness_needs_a_root_shell_pid() {
    let live = host(vec![
        (snapshot_call(), snapshot(22, &[PANE])),
        (process_call(), process_info(PANE, json!(4242))),
    ]);
    let observation = live.observe(pane());
    assert_eq!(observation.execution, ExecutionState::Live);
    assert_eq!(
        (observation.revision, observation.shell_pid),
        (Some(7), Some(4242))
    );
    assert_eq!(observation.liveness(), HostLiveness::Live);
    let unknown = host(vec![
        (snapshot_call(), snapshot(22, &[PANE])),
        (process_call(), process_info(PANE, Value::Null)),
    ]);
    assert_eq!(
        unknown.liveness(pane()),
        HostLiveness::ProbeError,
        "shell_pid null is not death"
    );
    let broken = host(vec![
        (snapshot_call(), snapshot(22, &[PANE])),
        (process_call(), not_sent()),
    ]);
    assert_eq!(broken.liveness(pane()), HostLiveness::ProbeError);
}

#[test]
fn herdr_process_info_from_another_connection_is_not_liveness() {
    let herdr = fake(
        vec![
            (snapshot_call(), snapshot(22, &[PANE])),
            (process_call(), process_info(PANE, json!(4242))),
        ],
        true,
    );
    let observation = herdr.observe(pane());
    assert_eq!(
        (observation.execution, observation.shell_pid),
        (ExecutionState::Unknown, None),
        "a pid read after a reconnect must not join the earlier snapshot"
    );
    assert_eq!(observation.liveness(), HostLiveness::ProbeError);
}

#[test]
fn herdr_observation_projection_truth_table() {
    use ControlPlane::{Incompatible, Reachable, Unreachable};
    use ExecutionState::{Dead, Live, Unknown};
    let obs = |control_plane, pane, execution| HerdrObservation {
        control_plane,
        pane,
        execution,
        revision: None,
        shell_pid: None,
    };
    for control_plane in [Unreachable, Incompatible] {
        let failed = obs(control_plane, PaneState::Missing, Dead);
        assert_eq!(failed.presence(), HostPresence::ProbeFailed);
        assert_eq!(failed.liveness(), HostLiveness::ProbeError);
    }
    for (pane, execution, presence, liveness) in [
        (
            PaneState::Present,
            Live,
            HostPresence::Present,
            HostLiveness::Live,
        ),
        (
            PaneState::Present,
            Dead,
            HostPresence::Present,
            HostLiveness::DeadOrAbsent,
        ),
        (
            PaneState::Present,
            Unknown,
            HostPresence::Present,
            HostLiveness::ProbeError,
        ),
        (
            PaneState::Missing,
            Unknown,
            HostPresence::Missing,
            HostLiveness::DeadOrAbsent,
        ),
        (
            PaneState::Unknown,
            Live,
            HostPresence::ProbeFailed,
            HostLiveness::ProbeError,
        ),
    ] {
        let observed = obs(Reachable, pane, execution);
        assert_eq!(observed.presence(), presence, "{pane:?}/{execution:?}");
        assert_eq!(observed.liveness(), liveness, "{pane:?}/{execution:?}");
    }
}

#[test]
fn herdr_execution_pid_is_the_shell_pid_not_a_foreground_pid() {
    let herdr = host(vec![(process_call(), process_info(PANE, json!(4242)))]);
    assert_eq!(
        herdr.execution_pid(pane()),
        Ok(Some(4242)),
        "execution_pid must map process_info.shell_pid"
    );
    let other_pane = host(vec![(process_call(), process_info("w9-9", json!(4242)))]);
    assert!(matches!(
        other_pane.execution_pid(pane()),
        Err(HostError::Protocol(_))
    ));
}

// catch_unwind rather than #[should_panic]: the test-lane parser does not read "- should panic" result lines.
#[test]
fn herdr_fake_transport_rejects_an_unscripted_request() {
    let herdr = host(vec![(
        HerdrRequest::PaneGet {
            pane_id: PANE.into(),
        },
        process_info(PANE, json!(4242)),
    )]);
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = herdr.execution_pid(pane());
    }))
    .expect_err("an unscripted request must fail the fake transport");
    let message = panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&str>().copied())
        .unwrap_or_default();
    assert!(
        message.contains("fake transport request mismatch"),
        "{message}"
    );
}

#[test]
fn herdr_working_dir_prefers_foreground_cwd() {
    let request = HerdrRequest::PaneGet {
        pane_id: PANE.into(),
    };
    let reply = Scripted::Result(json!({"type": "pane_info", "pane": pane_info(PANE)}));
    let herdr = host(vec![(request, reply)]);
    assert_eq!(
        herdr.current_working_dir(pane()),
        Ok(Some(PathBuf::from("/work/sub")))
    );
}

#[test]
fn herdr_capture_maps_scroll_back_and_rejects_truncation() {
    use HerdrReadSource::{RecentUnwrapped, Visible};
    for (scroll_back, source, wire_source, lines) in [
        (0, Visible, "visible", None),
        (i32::MAX, Visible, "visible", None),
        (-1, RecentUnwrapped, "recent_unwrapped", Some(1)),
        (-50, RecentUnwrapped, "recent_unwrapped", Some(50)),
        (-10_000, RecentUnwrapped, "recent_unwrapped", Some(10_000)),
        (-10_001, RecentUnwrapped, "recent_unwrapped", Some(10_000)),
        (i32::MIN, RecentUnwrapped, "recent_unwrapped", Some(10_000)),
    ] {
        let request = HerdrRequest::PaneRead {
            pane_id: PANE.into(),
            source,
            lines,
            strip_ansi: true,
        };
        let herdr = host(vec![(request, read(wire_source, false))]);
        assert_eq!(
            herdr.capture_screen(pane(), scroll_back),
            Ok("screen".to_string()),
            "scroll_back {scroll_back}"
        );
    }
    let visible = || HerdrRequest::PaneRead {
        pane_id: PANE.into(),
        source: Visible,
        lines: None,
        strip_ansi: true,
    };
    let truncated = host(vec![(visible(), read("visible", true))]);
    assert!(
        matches!(
            truncated.capture_screen(pane(), 0),
            Err(HostError::Protocol(_))
        ),
        "a truncated read must never be a complete screen"
    );
    let wrong_source = host(vec![(visible(), read("recent", false))]);
    assert!(matches!(
        wrong_source.capture_screen(pane(), 0),
        Err(HostError::Protocol(_))
    ));
}

#[test]
fn herdr_send_text_keeps_ambiguous_outcomes_indeterminate() {
    let herdr = host(vec![(send_call("안녕\n"), ok())]);
    assert_eq!(
        herdr.send_text(pane(), "안녕\n"),
        Ok(HostMutation::Confirmed)
    );
    let refused = host(vec![(send_call("x"), not_sent())]);
    assert_eq!(
        refused.send_text(pane(), "x"),
        Err(HostError::Transport("connect refused".into()))
    );
    let ambiguous: Vec<fn() -> Scripted> = vec![
        after_write,
        || Scripted::Error("internal"),
        || Scripted::WrongId,
        || snapshot(22, &[]),
    ];
    for outcome in ambiguous {
        let herdr = host(vec![(send_call("x"), outcome())]);
        assert!(
            matches!(
                herdr.send_text(pane(), "x"),
                Ok(HostMutation::Indeterminate(_))
            ),
            "a possibly delivered input must stay Indeterminate"
        );
        assert_eq!(calls(&herdr).len(), 1, "no automatic resend");
    }
}

#[test]
fn herdr_keys_and_non_herdr_refs_make_no_transport_call() {
    let herdr = host(Vec::new());
    let unsupported = |op| {
        Ok(HostMutation::Refused(HostRefusal::Unsupported {
            kind: HostKind::Herdr,
            op,
        }))
    };
    assert_eq!(herdr.send_keys(pane(), &["C-c"]), unsupported("send_keys"));
    assert_eq!(herdr.interrupt(pane()), unsupported("interrupt"));
    for wrong in [
        HostSessionRef::tmux(PANE),
        HostSessionRef::process(PANE),
        HostSessionRef::herdr_pane(" "),
    ] {
        assert_eq!(herdr.presence(wrong), HostPresence::ProbeFailed);
        assert_eq!(herdr.liveness(wrong), HostLiveness::ProbeError);
        assert!(herdr.send_text(wrong, "x").is_err());
        assert!(herdr.execution_pid(wrong).is_err());
    }
    assert!(calls(&herdr).is_empty());
    let caps = herdr.capabilities();
    assert!(caps.send_text && caps.capture_screen && !caps.send_keys && !caps.interrupt);
}

#[test]
fn herdr_endpoint_requires_every_field_and_an_absolute_socket() {
    let socket = Path::new("/tmp/h.sock");
    for (node, key, path, session) in [
        ("", "pilot", socket, "adk"),
        ("mac", " ", socket, "adk"),
        ("mac", "pilot", socket, ""),
        ("mac", "pilot", Path::new(""), "adk"),
        ("mac", "pilot", Path::new("h.sock"), "adk"),
    ] {
        assert_eq!(
            HerdrEndpoint::new(node, key, path, session),
            Err(HostError::Unsupported(HostKind::Herdr, ENDPOINT_MISSING)),
            "an incomplete endpoint must fail, never pick a default socket"
        );
    }
    let endpoint = HerdrEndpoint::new("mac", "pilot", socket, "adk").unwrap();
    let herdr = HerdrHost::new(endpoint.clone(), FakeTransport::default());
    assert_eq!(herdr.endpoint, endpoint);
    assert_eq!(
        (endpoint.herdr_session(), endpoint.socket_path()),
        ("adk", socket)
    );
}

#[test]
fn herdr_unconfigured_host_fails_every_call_explicitly() {
    let host = UnconfiguredHerdrHost;
    let missing = || HostError::Unsupported(HostKind::Herdr, ENDPOINT_MISSING);
    assert_eq!(host.kind(), HostKind::Herdr);
    assert_eq!(host.capabilities(), HostCapabilities::default());
    assert_eq!(host.presence(pane()), HostPresence::ProbeFailed);
    assert_eq!(host.liveness(pane()), HostLiveness::ProbeError);
    assert_eq!(host.send_text(pane(), "x"), Err(missing()));
    assert_eq!(host.send_keys(pane(), &["C-c"]), Err(missing()));
    assert_eq!(host.interrupt(pane()), Err(missing()));
    assert_eq!(host.capture_screen(pane(), 0), Err(missing()));
    assert_eq!(host.current_working_dir(pane()), Err(missing()));
    assert_eq!(host.execution_pid(pane()), Err(missing()));
}

#[test]
fn herdr_requests_serialize_with_schema_method_names() {
    let wire = |request| {
        serde_json::to_value(HerdrCall {
            id: "adk-1".into(),
            request,
        })
        .unwrap()
    };
    assert_eq!(
        wire(HerdrRequest::Ping {}),
        json!({"id": "adk-1", "method": "ping", "params": {}})
    );
    assert_eq!(
        wire(HerdrRequest::SessionSnapshot {}),
        json!({"id": "adk-1", "method": "session.snapshot", "params": {}})
    );
    assert_eq!(
        wire(HerdrRequest::PaneGet {
            pane_id: PANE.into()
        }),
        json!({"id": "adk-1", "method": "pane.get", "params": {"pane_id": PANE}})
    );
    assert_eq!(
        wire(process_call()),
        json!({"id": "adk-1", "method": "pane.process_info", "params": {"pane_id": PANE}})
    );
    assert_eq!(
        wire(contract::capture_request(PANE, -5)),
        json!({"id": "adk-1", "method": "pane.read", "params": {
            "pane_id": PANE, "source": "recent_unwrapped", "lines": 5, "strip_ansi": true
        }})
    );
    assert_eq!(
        wire(send_call("hi")),
        json!({"id": "adk-1", "method": "pane.send_text",
            "params": {"pane_id": PANE, "text": "hi"}})
    );
}

#[test]
fn herdr_reply_needs_exactly_one_of_result_or_error() {
    let parse = |body| serde_json::from_value::<HerdrReply>(body);
    assert!(parse(json!({"id": "a"})).is_err());
    assert!(
        parse(json!({"id": "a", "result": {"type": "ok"}, "error": {"code": "c", "message": "m"}}))
            .is_err()
    );
    assert!(parse(json!({"id": "a", "result": {"type": "session_snapshot"}})).is_err());
    assert!(
        parse(json!({"id": "a", "result": {"type": "pong", "version": "v"}})).is_err(),
        "a pong without protocol must not parse"
    );
    let other = parse(json!({"id": "a", "result": {"type": "tab_list", "tabs": []}}));
    assert!(matches!(
        other,
        Ok(HerdrReply {
            body: Ok(HerdrResult::Other),
            ..
        })
    ));
}

/// Lexer-aware end of a comment, string or char literal starting at `i`.
fn literal_end(chars: &[char], i: usize) -> Option<usize> {
    let at = |k: usize| chars.get(k).copied();
    let ident = |k: usize| k > 0 && at(k - 1).is_some_and(|c| c.is_alphanumeric() || c == '_');
    let find = |from: usize, pat: &[char]| {
        (from..chars.len())
            .find(|k| chars[*k..].starts_with(pat))
            .map(|k| k + pat.len())
    };
    match (at(i)?, at(i + 1)) {
        ('/', Some('/')) => Some(find(i, &['\n']).unwrap_or(chars.len())),
        ('/', Some('*')) => Some(find(i + 2, &['*', '/']).unwrap_or(chars.len())),
        ('"', _) => {
            let mut k = i + 1;
            while k < chars.len() && chars[k] != '"' {
                k += if chars[k] == '\\' { 2 } else { 1 };
            }
            Some(k + 1)
        }
        ('r', Some('"' | '#')) if !ident(i) => {
            let hashes = chars[i + 1..].iter().take_while(|c| **c == '#').count();
            if at(i + 1 + hashes) != Some('"') {
                return None;
            }
            let close: Vec<char> = std::iter::once('"').chain(vec!['#'; hashes]).collect();
            Some(find(i + 2 + hashes, &close).unwrap_or(chars.len()))
        }
        ('\'', Some('\\')) => find(i + 3, &['\'']),
        ('\'', _) if at(i + 2) == Some('\'') => Some(i + 3),
        _ => None,
    }
}

/// Source with every `#[cfg(test)]` item removed, plus the `mod x;` files it gated:
/// a `#[path]` value (relative to the file's directory) or the bare module name.
fn production_text(text: &str) -> (String, Vec<(String, Option<String>)>) {
    const GATE: &str = "#[cfg(test)]";
    let chars: Vec<char> = text.chars().collect();
    let gate: Vec<char> = GATE.chars().collect();
    let (mut out, mut test_mods, mut i) = (String::new(), Vec::new(), 0);
    while i < chars.len() {
        if let Some(end) = literal_end(&chars, i) {
            out.extend(&chars[i..end.min(chars.len())]);
            i = end;
        } else if chars[i..].starts_with(&gate) {
            // An item ends at `;`/`,` or its closing `}`; an enclosing closer ends it unconsumed.
            let (mut k, mut depth) = (i + gate.len(), 0usize);
            while k < chars.len() {
                if let Some(end) = literal_end(&chars, k) {
                    k = end;
                    continue;
                }
                match chars[k] {
                    '{' | '(' | '[' => depth += 1,
                    '}' | ')' | ']' if depth == 0 => break,
                    '}' if depth == 1 => {
                        k += 1;
                        break;
                    }
                    '}' | ')' | ']' => depth -= 1,
                    ';' | ',' if depth == 0 => {
                        let head: String = chars[i + gate.len()..k].iter().collect();
                        let words: Vec<&str> = head.split_whitespace().collect();
                        if let [.., "mod", name] = words.as_slice() {
                            let path = head
                                .split_once("#[path = \"")
                                .and_then(|(_, rest)| rest.split_once('"'));
                            test_mods
                                .push((name.to_string(), path.map(|(file, _)| file.to_string())));
                        }
                        k += 1;
                        break;
                    }
                    _ => {}
                }
                k += 1;
            }
            i = k;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    (out, test_mods)
}

/// Production text of every non-test `src/**/*.rs`, keyed by repo-relative path.
fn production_sources() -> BTreeMap<String, String> {
    let (probe, _) = production_text(
        "fn a() { b(\"}\"); }\n#[cfg(test)]\nmod t { const S: &str = \"{\"; }\nfn c() {}",
    );
    assert!(probe.contains("fn a()") && probe.contains("fn c()") && !probe.contains("mod t"));

    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut stack = vec![root.join("src")];
    let mut files = BTreeMap::new();
    let mut test_files = BTreeSet::new();
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry
                .expect("source scan: unreadable directory entry")
                .path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().is_none_or(|ext| ext != "rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap_or_else(|error| {
                panic!("source scan: unreadable {}: {error}", path.display())
            });
            let (prod, test_mods) = production_text(&text);
            let stem = path.file_stem().unwrap().to_string_lossy().to_string();
            let base = match stem.as_str() {
                "mod" | "lib" | "main" => path.parent().unwrap().to_path_buf(),
                _ => path.with_extension(""),
            };
            for (name, file) in test_mods {
                if let Some(file) = file {
                    test_files.insert(path.parent().unwrap().join(file));
                    continue;
                }
                test_files.insert(base.join(format!("{name}.rs")));
                test_files.insert(base.join(name).join("mod.rs"));
            }
            files.insert(path, (text.len(), prod));
        }
    }
    let (mut total, mut kept, mut sources) = (0, 0, BTreeMap::new());
    for (path, (len, prod)) in files {
        if test_files.contains(&path) {
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        (total, kept) = (total + len, kept + prod.len());
        sources.insert(relative, prod);
    }
    assert!(
        sources.len() > 100,
        "source scan found only {} files",
        sources.len()
    );
    assert!(
        kept * 2 > total,
        "test stripping kept only {kept} of {total} bytes"
    );
    sources
}

/// Code with comments blanked and literals emptied, so a scan sees only tokens.
fn code_tokens(prod: &str) -> String {
    let chars: Vec<char> = prod.chars().collect();
    let (mut out, mut i) = (String::new(), 0);
    while i < chars.len() {
        match literal_end(&chars, i) {
            Some(end) => {
                out.push_str(if chars[i] == '/' { " " } else { "\"\"" });
                i = end;
            }
            None => {
                out.push(chars[i]);
                i += 1;
            }
        }
    }
    out
}

/// Byte ranges of the `use` declarations in token-only code.
fn use_spans(code: &str) -> Vec<std::ops::Range<usize>> {
    let keyword = regex::Regex::new(r"\buse\b").unwrap();
    keyword
        .find_iter(code)
        .filter(|found| !code[..found.start()].ends_with("r#"))
        .map(|found| {
            let end = code[found.end()..].find(';');
            found.start()..end.map_or(code.len(), |k| found.end() + k)
        })
        .collect()
}

/// Uses of `name` other than its definition or a plain `use` import, plus the
/// names a `use … as` or `type … =` binds it to.
fn item_uses(code: &str, name: &str) -> (usize, Vec<String>) {
    if !code.contains(name) {
        return (0, Vec::new());
    }
    let word = regex::Regex::new(&format!(r"\b{}\b", regex::escape(name))).unwrap();
    let defines = |start: usize| {
        let before = code[..start].trim_end();
        let ident = |c: char| c.is_alphanumeric() || c == '_';
        before.len() < start
            && ["fn", "struct", "enum", "trait", "type", "mod"]
                .iter()
                .any(|k| {
                    before
                        .strip_suffix(k)
                        .is_some_and(|rest| !rest.ends_with(ident))
                })
    };
    let renamed = regex::Regex::new(r"^\s+as\s+(\w+)").unwrap();
    let type_alias = regex::Regex::new(&format!(
        r"\btype\s+(\w+)[^=;]*=[^;]*\b{}\b",
        regex::escape(name)
    ))
    .unwrap();
    let imports = use_spans(code);
    let mut aliases: Vec<String> = type_alias
        .captures_iter(code)
        .map(|c| c[1].to_string())
        .collect();
    let mut uses = 0;
    for found in word.find_iter(code) {
        if defines(found.start()) {
            continue;
        }
        if !imports.iter().any(|span| span.contains(&found.start())) {
            uses += 1;
        } else if let Some(alias) = renamed.captures(&code[found.end()..]) {
            aliases.extend((&alias[1] != "_").then(|| alias[1].to_string()));
        }
    }
    (uses, aliases)
}

const GUARD_ADAPTER: &str = "src/services/discord/inflight/host_recovery_guard.rs";

// Dormant guard: no production code reaches a Herdr host. Owners may only name
// Herdr items, never construct or route to one; everything else may not name them.
#[test]
fn herdr_items_have_no_production_caller() {
    const OWNERS: &[(&str, usize)] = &[
        ("src/services/session_host.rs", 0),
        ("src/services/session_host/herdr_host.rs", 5),
        ("src/services/session_host/herdr/model.rs", 1),
        ("src/services/session_host/herdr/contract.rs", 0),
        ("src/services/session_host/herdr/observe.rs", 0),
        ("src/services/session_host/herdr/transport.rs", 0),
        ("src/services/session_host/herdr/wire.rs", 0),
        ("src/services/session_host/model.rs", 3),
        ("src/services/session_host/resolve.rs", 2),
        ("src/services/session_host/consumer_guard.rs", 2),
        ("src/services/session_host/legacy_collapse.rs", 1),
        ("src/services/session_host/tmux_host.rs", 0),
        ("src/services/session_host/process_host.rs", 0),
        ("src/services/discord/inflight/host_locator.rs", 1),
        ("src/services/provider/session_probe.rs", 2),
        (GUARD_ADAPTER, 1),
    ];
    const NEEDLES: &[&str] = &[
        "HerdrHost",
        "HerdrEndpoint",
        "HerdrTransport",
        "HerdrSocket",
        "HostKind::Herdr",
        "herdr_pane(",
        "session_host::herdr",
    ];
    const ACTIVATIONS: &[&str] = &[
        "host_for(HostKind::Herdr",
        "HerdrHost::new(",
        "HerdrHost::<",
        "HerdrSocketTransport::new(",
        "HerdrSocketTransport::<",
    ];
    // Nothing writes a host locator or `.host_kind` marker; only their owners, the guard
    // adapter and the cleanup gate read one. Termination holds a locator only as a target.
    const LOCATOR: &str = "src/services/discord/inflight/host_locator.rs";
    const MARKER: &str = "src/services/tmux_common/host_marker.rs";
    const INFLIGHT_MODEL: &str = "src/services/discord/inflight/model.rs";
    const CLEANUP_GATE: &str = "src/db/dispatched_sessions/hosted_execution.rs";
    const READERS: &[(&str, &[&str])] = &[
        (
            "PersistedHostLocator",
            &[LOCATOR, INFLIGHT_MODEL, GUARD_ADAPTER],
        ),
        (
            "HostedRuntimeLocator",
            &[
                LOCATOR,
                "src/services/session_host.rs",
                "src/services/session_host/model.rs",
                "src/services/termination_audit/host_terminate.rs",
            ],
        ),
        ("HostKind::from_persisted", &[LOCATOR, MARKER]),
        ("HostKindMarker", &[MARKER, GUARD_ADAPTER, CLEANUP_GATE]),
        ("read_host_kind_marker", &[MARKER, CLEANUP_GATE]),
        ("host_marker::", &[GUARD_ADAPTER, CLEANUP_GATE]),
        (".host_locator", &[GUARD_ADAPTER]),
        ("host_locator: Some", &[]),
        ("host_locator:", &[INFLIGHT_MODEL, GUARD_ADAPTER]),
    ];
    let variant = regex::Regex::new(r"\bHerdr\b|\*").unwrap();
    let mut violations = Vec::new();
    for (relative, prod) in &production_sources() {
        let relative = relative.as_str();
        let owner = OWNERS.iter().find(|(owner, _)| *owner == relative);
        let named = match owner {
            Some(_) => Vec::new(),
            None => NEEDLES.iter().filter(|n| prod.contains(**n)).collect(),
        };
        let activated = ACTIVATIONS.iter().filter(|n| prod.contains(**n));
        violations.extend(
            named
                .into_iter()
                .chain(activated)
                .map(|n| format!("{relative}: {n}")),
        );
        let code = code_tokens(prod);
        if item_uses(&code, "herdr_pane").0 > 0 {
            violations.push(format!("{relative}: herdr_pane use"));
        }
        // `HostKind::{Herdr}` or a glob import would name the variant without its path.
        if owner.is_none()
            && use_spans(&code).into_iter().any(|span| {
                code[span.clone()].contains("HostKind") && variant.is_match(&code[span])
            })
        {
            violations.push(format!("{relative}: HostKind::Herdr import"));
        }
        violations.extend(
            READERS
                .iter()
                .filter(|(n, owners)| !owners.contains(&relative) && prod.contains(*n))
                .map(|(n, _)| format!("{relative}: {n}")),
        );
        let fields = prod.matches("host_locator:").count() - prod.matches("host_locator::").count();
        if relative == INFLIGHT_MODEL && fields > 2 {
            violations.push(format!("{relative}: host_locator: x{fields} > 2"));
        }
        let routed = prod.matches("HostKind::Herdr").count();
        if let Some((_, ceiling)) = owner.filter(|(_, ceiling)| routed > *ceiling) {
            violations.push(format!("{relative}: HostKind::Herdr x{routed} > {ceiling}"));
        }
    }
    assert!(
        violations.is_empty(),
        "Herdr production caller: {violations:?}"
    );
}

// Dormant guard: the session target resolver and consumer guard have no production
// caller. Their owners may only define them; nothing else may name them or an alias.
#[test]
fn session_target_guard_has_no_production_caller() {
    const RESOLVE: &str = "src/services/session_host/resolve.rs";
    const GUARD: &str = "src/services/session_host/consumer_guard.rs";
    const ROOT: &str = "src/services/session_host.rs";
    const INPUT: &str = "src/services/claude_tui/host_input.rs";
    // Needle, files that may name it, and the calls allowed there beyond its `fn`.
    const ITEMS: &[(&str, &[&str], usize)] = &[
        ("resolve_session_target", &[RESOLVE, ROOT], 0),
        ("resolve_target_host", &[RESOLVE], 1),
        ("legacy_target_host", &[RESOLVE], 1),
        ("guard_first_state_change", &[GUARD, ROOT], 0),
        ("probe_for_policy", &[GUARD, ROOT], 0),
        ("legacy_ref", &[RESOLVE, GUARD], 1),
        ("with_inflight_row", &[GUARD_ADAPTER], 0),
        ("locator_witness", &[GUARD_ADAPTER], 1),
        ("marker_witness", &[GUARD_ADAPTER], 0),
        (
            "ResolvedSessionTarget",
            &[RESOLVE, GUARD, ROOT, INPUT],
            usize::MAX,
        ),
        ("from_session_target", &[INPUT], 0),
        (
            "SessionTargetEvidence",
            &[RESOLVE, ROOT, GUARD_ADAPTER],
            usize::MAX,
        ),
        ("SessionTargetInput", &[RESOLVE, ROOT], usize::MAX),
        ("HostWitness", &[RESOLVE, ROOT, GUARD_ADAPTER], usize::MAX),
        ("GuardVerdict", &[GUARD, ROOT], usize::MAX),
        ("PolicyProbe", &[GUARD, ROOT], usize::MAX),
        ("consumer_guard", &[ROOT], usize::MAX),
        (
            "host_recovery_guard",
            &["src/services/discord/inflight.rs"],
            usize::MAX,
        ),
    ];
    let sources = production_sources();
    let codes: BTreeMap<&str, String> = sources
        .iter()
        .map(|(relative, prod)| (relative.as_str(), code_tokens(prod)))
        .collect();
    // An alias inherits its item's owners and use budget and is matched as a whole word.
    let mut items: Vec<(String, &[&str], usize, bool)> = ITEMS
        .iter()
        .map(|(needle, owners, calls)| (needle.to_string(), *owners, *calls, false))
        .collect();
    let mut violations = Vec::new();
    let mut next = 0;
    while let Some((needle, owners, calls, alias)) = items.get(next).cloned() {
        next += 1;
        for (relative, code) in &codes {
            let (used, aliases) = item_uses(code, &needle);
            for renamed in aliases {
                if !items.iter().any(|(known, ..)| *known == renamed) {
                    items.push((renamed, owners, calls, true));
                }
            }
            let named = if alias {
                used > 0
            } else {
                sources[*relative].contains(needle.as_str())
            };
            if !named {
                continue;
            }
            if !owners.contains(relative) {
                violations.push(format!("{relative}: {needle}"));
            } else if calls != usize::MAX && used > calls {
                violations.push(format!("{relative}: {needle} used x{used} > {calls}"));
            }
        }
    }
    let guard = &sources[GUARD];
    assert!(
        guard.contains("fn guard_first_state_change(")
            && sources[RESOLVE].contains("fn resolve_session_target("),
        "source scan must see the guarded definitions"
    );
    assert!(
        violations.is_empty(),
        "session target guard production caller: {violations:?}"
    );
}

// Dormant guard: the typed probe entries have no production caller outside their
// owner, so no consumer reads host liveness through them yet.
#[test]
fn typed_session_probe_entries_have_no_production_caller() {
    const OWNER: &str = "src/services/provider/session_probe.rs";
    const ENTRIES: &[&str] = &[
        "SessionProbeTarget",
        "SessionProbe::for_target",
        "observe_session_liveness",
    ];
    let sources = production_sources();
    let owner = &sources[OWNER];
    assert!(
        owner.contains("fn observe_session_liveness(") && owner.contains("fn for_target("),
        "source scan must see the typed entries"
    );
    let violations: Vec<String> = sources
        .iter()
        .filter(|(relative, _)| relative.as_str() != OWNER)
        .flat_map(|(relative, prod)| {
            ENTRIES
                .iter()
                .filter(|entry| prod.contains(**entry))
                .map(move |entry| format!("{relative}: {entry}"))
        })
        .collect();
    assert!(
        violations.is_empty(),
        "typed probe production caller: {violations:?}"
    );
}
