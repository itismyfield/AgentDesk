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
        ("src/services/session_host/resolve.rs", 1),
        ("src/services/session_host/legacy_collapse.rs", 1),
        ("src/services/session_host/tmux_host.rs", 0),
        ("src/services/session_host/process_host.rs", 0),
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
    let (probe, _) = production_text(
        "fn a() { b(\"}\"); }\n#[cfg(test)]\nmod t { const S: &str = \"{\"; }\nfn c() {}",
    );
    assert!(probe.contains("fn a()") && probe.contains("fn c()") && !probe.contains("mod t"));

    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut stack = vec![root.join("src")];
    let mut files = BTreeMap::new();
    let mut test_files = BTreeSet::new();
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().is_none_or(|ext| ext != "rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap_or_default();
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
    let (mut total, mut kept, mut violations) = (0, 0, Vec::new());
    for (path, (len, prod)) in &files {
        if test_files.contains(path) {
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        (total, kept) = (total + len, kept + prod.len());
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
        if prod.matches("herdr_pane(").count() > prod.matches("fn herdr_pane(").count() {
            violations.push(format!("{relative}: herdr_pane( call"));
        }
        let routed = prod.matches("HostKind::Herdr").count();
        if let Some((_, ceiling)) = owner.filter(|(_, ceiling)| routed > *ceiling) {
            violations.push(format!("{relative}: HostKind::Herdr x{routed} > {ceiling}"));
        }
    }
    assert!(
        files.len() > 100,
        "source scan found only {} files",
        files.len()
    );
    assert!(
        kept * 2 > total,
        "test stripping kept only {kept} of {total} bytes"
    );
    assert!(
        violations.is_empty(),
        "Herdr production caller: {violations:?}"
    );
}
