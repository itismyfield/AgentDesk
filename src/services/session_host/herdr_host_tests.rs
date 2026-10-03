#![cfg(unix)]

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;
use std::sync::Mutex;

use serde_json::{Value, json};

use super::*;
use crate::services::session_host::herdr::contract::{
    HerdrTransportError, ServerWitness, Witnessed,
};
use crate::services::session_host::herdr::model::{
    ExecutionState, HerdrReadSource, HerdrReply, HerdrResult, PaneState,
};
use crate::services::session_host::herdr::observe::RestoreUnverified;
use crate::services::session_host::herdr::provenance::StartIdentity;

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
    /// The server each call's connection reaches, in order; once empty, `serving()`.
    reached: Mutex<VecDeque<Witnessed>>,
    /// E7 readings in order; once empty, Off naming `serving()`.
    restore: Mutex<VecDeque<RestoreResume>>,
}

/// The server a fake connection reaches unless a test scripts another.
fn serving() -> ServerWitness {
    ServerWitness {
        socket: "/tmp/h.sock".into(),
        pid: 4,
        start: StartIdentity::Darwin {
            seconds: 1_000,
            micros: 0,
        },
    }
}

impl HerdrTransport for FakeTransport {
    fn call(&self, call: &HerdrCall) -> (contract::HerdrOutcome, Witnessed) {
        self.calls.lock().unwrap().push(call.clone());
        (self.reply(call), self.reach())
    }

    fn call_with_witness(
        &self,
        call: &HerdrCall,
        expected: &ServerWitness,
    ) -> contract::HerdrOutcome {
        let reached = self.reach();
        if reached.as_ref() != Ok(expected) {
            return Err(HerdrTransportError::NotSent(format!("{reached:?}")));
        }
        self.calls.lock().unwrap().push(call.clone());
        self.reply(call)
    }
}

fn scripted_restore(transport: &FakeTransport, _endpoint: &HerdrEndpoint) -> RestoreResume {
    let next = transport.restore.lock().unwrap().pop_front();
    next.unwrap_or(RestoreResume::Off { witness: serving() })
}

impl FakeTransport {
    fn reach(&self) -> Witnessed {
        let next = self.reached.lock().unwrap().pop_front();
        next.unwrap_or_else(|| Ok(serving()))
    }

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

fn fake(script: Vec<Step>, reached: Vec<Witnessed>) -> HerdrHost<FakeTransport> {
    let endpoint = HerdrEndpoint::new("mac-mini", "pilot", Path::new("/tmp/h.sock"), "adk")
        .expect("valid endpoint");
    let transport = FakeTransport {
        script: Mutex::new(script.into()),
        reached: Mutex::new(reached.into()),
        ..FakeTransport::default()
    };
    HerdrHost::new(endpoint, transport).with_restore_reader(scripted_restore)
}

fn host(script: Vec<Step>) -> HerdrHost<FakeTransport> {
    fake(script, Vec::new())
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
fn herdr_process_info_from_another_server_is_not_liveness() {
    let restarted = ServerWitness {
        start: StartIdentity::Darwin {
            seconds: 1_001,
            micros: 0,
        },
        ..serving()
    };
    for reached in [
        vec![Ok(serving()), Ok(restarted)],
        vec![Ok(serving()), Err(RestoreUnverified::ProcessUnreadable)],
        vec![
            Err(RestoreUnverified::NoPeer),
            Err(RestoreUnverified::NoPeer),
        ],
    ] {
        let herdr = fake(
            vec![
                (snapshot_call(), snapshot(22, &[PANE])),
                (process_call(), process_info(PANE, json!(4242))),
            ],
            reached.clone(),
        );
        let observation = herdr.observe(pane());
        assert_eq!(observation.presence(), HostPresence::Present);
        assert_eq!(
            (observation.execution, observation.shell_pid),
            (ExecutionState::Unknown, None),
            "a pid from another or unnamed server must not join the snapshot: {reached:?}"
        );
        assert_eq!(observation.liveness(), HostLiveness::ProbeError);
    }
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

fn keys_call(keys: &[&str]) -> HerdrRequest {
    HerdrRequest::PaneSendKeys {
        pane_id: PANE.into(),
        keys: keys.iter().map(|key| key.to_string()).collect(),
    }
}

// Executor keys reach Herdr under its names in one request, a paste is bracketed and a line
// carries its Enter; an unknown key, a bad text or a non-Herdr ref sends nothing at all.
#[test]
fn herdr_keys_map_whole_or_send_nothing() {
    let executor_keys = ["Enter", "Escape", "C-u", "C-e", "Left", "Right", "BSpace"];
    let herdr_names = [
        "enter",
        "esc",
        "ctrl+u",
        "ctrl+e",
        "left",
        "right",
        "backspace",
    ];
    let herdr = host(vec![
        (keys_call(&herdr_names), ok()),
        (keys_call(&["ctrl+c"]), ok()),
        (keys_call(&["esc"]), Scripted::Error("invalid_key")),
        (send_call("\x1b[200~한글\n둘째\x1b[201~"), ok()),
        (
            HerdrRequest::PaneSendInput {
                pane_id: PANE.into(),
                text: "/clear".into(),
                keys: vec!["enter".into()],
            },
            ok(),
        ),
    ]);
    assert_eq!(
        herdr.send_keys(pane(), &executor_keys),
        Ok(HostMutation::Confirmed)
    );
    assert_eq!(herdr.interrupt(pane()), Ok(HostMutation::Confirmed));
    assert!(matches!(
        herdr.send_keys(pane(), &["Escape"]),
        Ok(HostMutation::Indeterminate(_))
    ));
    assert_eq!(
        herdr.send_paste(pane(), "한글\n둘째"),
        Ok(HostMutation::Confirmed)
    );
    assert_eq!(
        herdr.send_line(pane(), "/clear"),
        Ok(HostMutation::Confirmed)
    );
    assert_eq!(calls(&herdr).len(), 5);

    let silent = host(Vec::new());
    let unsupported = Ok(HostMutation::Refused(HostRefusal::Unsupported {
        kind: HostKind::Herdr,
        op: "send_keys",
    }));
    for keys in [&["Enter", "C-z"][..], &["enter"], &[]] {
        assert_eq!(silent.send_keys(pane(), keys), unsupported, "{keys:?}");
    }
    let end = format!("a{PASTE_END}b");
    for refused in [
        silent.send_paste(pane(), &end),
        silent.send_line(pane(), "/clear\n"),
        silent.send_line(pane(), ""),
    ] {
        assert!(
            matches!(
                refused,
                Ok(HostMutation::Refused(HostRefusal::Precondition(_)))
            ),
            "{refused:?}"
        );
    }
    for wrong in [
        HostSessionRef::tmux(PANE),
        HostSessionRef::process(PANE),
        HostSessionRef::herdr_pane(" "),
    ] {
        assert_eq!(silent.presence(wrong), HostPresence::ProbeFailed);
        assert_eq!(silent.liveness(wrong), HostLiveness::ProbeError);
        assert!(silent.send_text(wrong, "x").is_err());
        assert!(silent.send_keys(wrong, &["Enter"]).is_err());
        assert!(silent.send_paste(wrong, "x").is_err());
        assert!(silent.execution_pid(wrong).is_err());
    }
    assert!(calls(&silent).is_empty());
}

/// Text, Enter, draft clear (C-e, C-u), cancel (Escape, interrupt), paste and a command line.
fn every_input(herdr: &HerdrHost<FakeTransport>) -> Vec<Result<HostMutation, HostError>> {
    let mut outcomes = vec![herdr.send_text(pane(), "x")];
    for keys in [&["Enter"][..], &["C-e", "C-u"], &["Escape"]] {
        outcomes.push(herdr.send_keys(pane(), keys));
    }
    outcomes.push(herdr.interrupt(pane()));
    outcomes.push(herdr.send_paste(pane(), "x"));
    outcomes.push(herdr.send_line(pane(), "/clear"));
    outcomes
}

fn restore_refused() -> Result<HostMutation, HostError> {
    Ok(HostMutation::Refused(HostRefusal::Precondition(
        RESTORE_RESUME_NOT_OFF.into(),
    )))
}

#[test]
fn herdr_production_restore_reader_refuses_every_input_before_any_call() {
    let endpoint = HerdrEndpoint::new("mac-mini", "pilot", Path::new("/tmp/h.sock"), "adk")
        .expect("valid endpoint");
    let herdr = HerdrHost::new(endpoint, FakeTransport::default());
    for outcome in every_input(&herdr) {
        assert_eq!(
            outcome,
            restore_refused(),
            "no effective-config read exists"
        );
    }
    assert!(
        calls(&herdr).is_empty(),
        "an unverified server gets no input"
    );
}

#[test]
fn herdr_input_needs_a_fresh_off_reading_naming_the_server_that_gets_it() {
    let with_readings = |readings: Vec<RestoreResume>| {
        let herdr = host(vec![(send_call("x"), ok())]);
        *herdr.transport.restore.lock().unwrap() = readings.into();
        herdr
    };
    for reading in [
        RestoreResume::On,
        RestoreResume::Unverified(RestoreUnverified::NoPeer),
    ] {
        let herdr = with_readings(vec![reading.clone(); 8]);
        for outcome in every_input(&herdr) {
            assert_eq!(outcome, restore_refused(), "{reading:?}");
        }
        assert!(calls(&herdr).is_empty(), "{reading:?}");
    }

    let off = RestoreResume::Off { witness: serving() };
    let herdr = with_readings(vec![
        off.clone(),
        RestoreResume::Unverified(RestoreUnverified::NoPeer),
    ]);
    assert_eq!(herdr.send_text(pane(), "x"), Ok(HostMutation::Confirmed));
    assert_eq!(
        herdr.send_text(pane(), "x"),
        restore_refused(),
        "an earlier Off is not reused"
    );
    assert_eq!(calls(&herdr).len(), 1);

    let replaced = ServerWitness {
        pid: 5,
        ..serving()
    };
    let herdr = with_readings(vec![off]);
    *herdr.transport.reached.lock().unwrap() = vec![Ok(replaced)].into();
    assert!(
        matches!(herdr.send_text(pane(), "x"), Err(HostError::Transport(_))),
        "a reading of the replaced server admits nothing on the new one"
    );
    assert!(calls(&herdr).is_empty());
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

/// Lexer-aware end of a comment, string or char literal starting at `i`. A comment
/// or literal that never closes ends past `chars`, so callers can refuse the file.
fn literal_end(chars: &[char], i: usize) -> Option<usize> {
    let at = |k: usize| chars.get(k).copied();
    let ident = |k: usize| k > 0 && at(k - 1).is_some_and(|c| c.is_alphanumeric() || c == '_');
    let unclosed = chars.len() + 1;
    let find = |from: usize, pat: &[char]| {
        (from..chars.len())
            .find(|k| chars[*k..].starts_with(pat))
            .map_or(unclosed, |k| k + pat.len())
    };
    match (at(i)?, at(i + 1)) {
        ('/', Some('/')) => Some(find(i, &['\n']).min(chars.len())),
        ('/', Some('*')) => {
            let (mut k, mut depth) = (i + 2, 1);
            while depth > 0 {
                if k >= chars.len() {
                    return Some(unclosed);
                }
                let step =
                    chars[k..].starts_with(&['/', '*']) || chars[k..].starts_with(&['*', '/']);
                if step {
                    depth = if chars[k] == '/' {
                        depth + 1
                    } else {
                        depth - 1
                    };
                }
                k += if step { 2 } else { 1 };
            }
            Some(k)
        }
        ('"', _) => {
            let mut k = i + 1;
            while k < chars.len() && chars[k] != '"' {
                k += if chars[k] == '\\' { 2 } else { 1 };
            }
            Some(if k < chars.len() { k + 1 } else { unclosed })
        }
        // `r"…"`, `r#"…"#` and the byte and C forms `br#"…"#`, `cr#"…"#`.
        ('r', Some('"' | '#'))
            if !ident(i) || (matches!(at(i - 1), Some('b' | 'c')) && !ident(i - 1)) =>
        {
            let hashes = chars[i + 1..].iter().take_while(|c| **c == '#').count();
            if at(i + 1 + hashes) != Some('"') {
                return None;
            }
            let close: Vec<char> = std::iter::once('"').chain(vec!['#'; hashes]).collect();
            Some(find(i + 2 + hashes, &close))
        }
        ('\'', Some('\\')) => Some(find(i + 3, &['\''])),
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

/// Code with comments blanked and literals emptied, so a scan sees only tokens;
/// `None` when a comment or literal never closes.
fn closed_code_tokens(prod: &str) -> Option<String> {
    let chars: Vec<char> = prod.chars().collect();
    let (mut out, mut i) = (String::new(), 0);
    while i < chars.len() {
        // Only these characters can open a comment or literal.
        let opens = matches!(chars[i], '/' | '"' | 'r' | '\'');
        match opens.then(|| literal_end(&chars, i)).flatten() {
            Some(end) if end > chars.len() => return None,
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
    Some(out)
}

/// Token-only code for scans of one file; an unclosed literal empties it, and the
/// whole-tree scans report that file.
fn code_tokens(prod: &str) -> String {
    closed_code_tokens(prod).unwrap_or_default()
}

/// Byte ranges of the `use` declarations in token-only code.
fn use_spans(code: &str) -> Vec<std::ops::Range<usize>> {
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    code.match_indices("use")
        .map(|(start, _)| (start, start + 3))
        .filter(|&(start, end)| {
            let before = &code[..start];
            !before.ends_with(ident) && !before.ends_with("r#") && !code[end..].starts_with(ident)
        })
        .map(|(start, end)| start..code[end..].find(';').map_or(code.len(), |k| end + k))
        .collect()
}

/// One name a `use` tree binds: the full path and the local name, `None` for a glob.
#[derive(Debug)]
struct UseLeaf {
    path: Vec<String>,
    binds: Option<String>,
}

fn use_tree(tokens: &[&str], i: &mut usize, mut path: Vec<String>, out: &mut Vec<UseLeaf>) {
    loop {
        match tokens.get(*i).copied() {
            Some("::") => *i += 1,
            Some("{") => {
                *i += 1;
                while !matches!(tokens.get(*i).copied(), None | Some("}")) {
                    let start = *i;
                    use_tree(tokens, i, path.clone(), out);
                    if tokens.get(*i) == Some(&",") || *i == start {
                        *i += 1;
                    }
                }
                *i += 1;
                return;
            }
            Some("*") => {
                *i += 1;
                out.push(UseLeaf { path, binds: None });
                return;
            }
            Some(word) if !matches!(word, "as" | "," | "}" | ";") => {
                path.push(word.trim_start_matches("r#").to_string());
                *i += 1;
                if tokens.get(*i) != Some(&"::") {
                    break;
                }
            }
            _ => break,
        }
    }
    if path.last().is_some_and(|last| last == "self") {
        path.pop();
    }
    let binds = if tokens.get(*i) == Some(&"as") {
        *i += 2;
        tokens.get(*i - 1).map(|name| name.to_string())
    } else {
        path.last().cloned()
    };
    out.push(UseLeaf { path, binds });
}

/// Every name the `use` declarations of token-only code bind.
fn use_leaves(code: &str) -> Vec<UseLeaf> {
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    let mut leaves = Vec::new();
    for span in use_spans(code) {
        let (text, mut tokens) = (&code[span], Vec::new());
        let mut rest = text.trim_start();
        while let Some(first) = rest.chars().next() {
            let len = if rest.starts_with("::") {
                2
            } else if rest.starts_with("r#") || ident(first) {
                let body = rest.strip_prefix("r#").unwrap_or(rest);
                rest.len() - body.len() + body.find(|c: char| !ident(c)).unwrap_or(body.len())
            } else {
                first.len_utf8()
            };
            tokens.push(&rest[..len]);
            rest = rest[len..].trim_start();
        }
        // Token 0 is the `use` keyword itself.
        use_tree(&tokens, &mut 1, Vec::new(), &mut leaves);
    }
    leaves
}

/// Module path of a source file: `src/a/b.rs` and `src/a/b/mod.rs` are both `a::b`.
fn module_of(relative: &str) -> Vec<String> {
    let path = relative.strip_prefix("src/").unwrap_or(relative);
    let mut segments: Vec<String> = path
        .trim_end_matches(".rs")
        .split('/')
        .map(str::to_string)
        .collect();
    if matches!(
        segments.last().map(String::as_str),
        Some("mod" | "lib" | "main")
    ) {
        segments.pop();
    }
    segments
}

/// Absolute module path that `path`, written inside `module`, names.
fn absolute_path(module: &[String], path: &[String]) -> Vec<String> {
    let (mut base, mut rest) = (module.to_vec(), path);
    match path.first().map(String::as_str) {
        Some("crate") => (base, rest) = (Vec::new(), &path[1..]),
        Some("self") => rest = &path[1..],
        _ => {}
    }
    while rest.first().is_some_and(|segment| segment == "super") {
        base.pop();
        rest = &rest[1..];
    }
    base.extend(rest.iter().cloned());
    base
}

/// Token-only view of one production file.
struct ScannedFile {
    code: String,
    module: Vec<String>,
    leaves: Vec<UseLeaf>,
}

impl ScannedFile {
    /// Whether a bare name bound in `module` is in scope here: declared here or glob-imported.
    fn sees_bare(
        &self,
        files: &BTreeMap<String, ScannedFile>,
        here: bool,
        module: &[String],
    ) -> bool {
        here || self.globs_reach(files, module)
    }

    /// Whether the glob imports here reach `module`, also through modules that glob-import it
    /// in turn; a crate-relative or aliased glob no scanned file answers for may reach it.
    fn globs_reach(&self, files: &BTreeMap<String, ScannedFile>, module: &[String]) -> bool {
        let (mut seen, mut queue) = (vec![self.module.clone()], vec![self]);
        while let Some(file) = queue.pop() {
            for leaf in file.leaves.iter().filter(|leaf| leaf.binds.is_none()) {
                let target = resolve_path(files, file, &leaf.path);
                if target == module {
                    return true;
                }
                match files.values().find(|other| other.module == target) {
                    Some(next) if !seen.contains(&target) => {
                        seen.push(target);
                        queue.push(next);
                    }
                    Some(_) => {}
                    None => {
                        let head = leaf.path.first().map(String::as_str);
                        let aliased = head.is_some_and(|head| file.bound(head).is_some());
                        if aliased || matches!(head, Some("crate" | "self" | "super")) {
                            return true;
                        }
                    }
                }
            }
        }
        false
    }

    /// The path a `use` here binds `name` to, unless it binds the bare name itself.
    fn bound(&self, name: &str) -> Option<&[String]> {
        self.leaves
            .iter()
            .find(|leaf| leaf.binds.as_deref() == Some(name) && leaf.path != [name])
            .map(|leaf| leaf.path.as_slice())
    }
}

/// Absolute path `path` names in `file`, following a leading `use`-bound name and any module
/// another file re-exports under a new name, whether the path starts at a `use` or `crate`.
fn resolve_path(
    files: &BTreeMap<String, ScannedFile>,
    file: &ScannedFile,
    path: &[String],
) -> Vec<String> {
    let local = |scope: &ScannedFile, path: &[String]| {
        let mut path = path.to_vec();
        for _ in 0..8 {
            let head = path.first().map(String::as_str);
            let Some(bound) = head
                .filter(|head| !matches!(*head, "crate" | "self" | "super"))
                .and_then(|head| scope.bound(head))
            else {
                break;
            };
            path = bound.iter().chain(&path[1..]).cloned().collect();
        }
        absolute_path(&scope.module, &path)
    };
    let mut resolved = local(file, path);
    for _ in 0..8 {
        let reexport = (1..resolved.len()).find_map(|k| {
            let owner = files.values().find(|other| other.module == resolved[..k])?;
            Some((k, owner, owner.bound(&resolved[k])?))
        });
        let Some((k, owner, bound)) = reexport else {
            break;
        };
        let mut next = local(owner, bound);
        next.extend(resolved[k + 1..].iter().cloned());
        resolved = next;
    }
    resolved
}

/// Every file tokenized; a file whose comment or literal never closes is a violation.
fn scan_files(
    sources: &BTreeMap<String, String>,
    violations: &mut Vec<String>,
) -> BTreeMap<String, ScannedFile> {
    let mut files = BTreeMap::new();
    for (relative, prod) in sources {
        let Some(code) = closed_code_tokens(prod) else {
            violations.push(format!("{relative}: unterminated comment or literal"));
            continue;
        };
        let leaves = use_leaves(&code);
        let module = module_of(relative);
        files.insert(
            relative.clone(),
            ScannedFile {
                code,
                module,
                leaves,
            },
        );
    }
    files
}

/// Path segments written before the name at `start`, as in `a::b::name`.
fn qualifier(code: &str, start: usize) -> Vec<String> {
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    let (mut prefix, mut end) = (Vec::new(), start);
    while let Some(rest) = code[..end].trim_end().strip_suffix("::") {
        let rest = rest.trim_end();
        let begin = rest
            .char_indices()
            .rev()
            .find(|(_, c)| !ident(*c))
            .map_or(0, |(k, c)| k + c.len_utf8());
        if begin == rest.len() {
            break;
        }
        prefix.insert(0, rest[begin..].to_string());
        end = begin;
    }
    prefix
}

/// Qualifier path of each whole-word `name` outside `use` declarations and definitions.
fn word_uses(code: &str, name: &str) -> Vec<Vec<String>> {
    word_sites(code, name)
        .into_iter()
        .map(|(prefix, _)| prefix)
        .collect()
}

/// [`word_uses`], with whether a call follows each use; any other use takes it as a value.
fn word_sites(code: &str, name: &str) -> Vec<(Vec<String>, bool)> {
    if !code.contains(name) {
        return Vec::new();
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
    let imports = use_spans(code);
    let shadowed = shadowed_spans(code, name, &imports);
    word.find_iter(code)
        .filter(|found| {
            !defines(found.start()) && !imports.iter().any(|span| span.contains(&found.start()))
        })
        .map(|found| (found.range(), qualifier(code, found.start())))
        .filter(|(at, prefix)| {
            !prefix.is_empty() || !shadowed.iter().any(|s| s.contains(&at.start))
        })
        .map(|(at, prefix)| {
            let after = code[at.end..].trim_start();
            (prefix, after.starts_with('(') || after.starts_with("::<"))
        })
        .collect()
}

/// Where a `let` rebinds `name`: its own name, then its block after the statement, cut short
/// by a `use` naming `name` again. An initializer naming `name` binds the item, so no shadow.
fn shadowed_spans(
    code: &str,
    name: &str,
    imports: &[std::ops::Range<usize>],
) -> Vec<std::ops::Range<usize>> {
    if !code.contains("let") {
        return Vec::new();
    }
    let escaped = regex::escape(name);
    let binding = regex::Regex::new(&format!(r"\blet\s+(?:mut\s+)?({escaped})\b")).unwrap();
    let word = regex::Regex::new(&format!(r"\b{escaped}\b")).unwrap();
    let mut spans = Vec::new();
    for found in binding.captures_iter(code) {
        let bound = found.get(1).unwrap();
        spans.push(bound.range());
        let (mut depth, mut statement_end) = (0usize, None);
        let mut end = code.len();
        for (offset, ch) in code[bound.end()..].char_indices() {
            let at = bound.end() + offset;
            match ch {
                '{' | '(' | '[' => depth += 1,
                '}' | ')' | ']' if depth == 0 => {
                    end = at;
                    break;
                }
                '}' | ')' | ']' => depth -= 1,
                ';' if depth == 0 && statement_end.is_none() => statement_end = Some(at),
                _ => {}
            }
        }
        let Some(start) = statement_end else {
            continue;
        };
        if word.is_match(&code[bound.end()..start]) {
            continue;
        }
        let reimport = imports
            .iter()
            .filter(|span| span.start > start && span.start < end)
            .find(|span| word.is_match(&code[(*span).clone()]));
        spans.push(start..reimport.map_or(end, |span| span.start));
    }
    spans
}

/// Uses of `name` in one file's code, plus the names its `use … as` or `type … =` bind it to.
fn item_uses(code: &str, name: &str) -> (usize, Vec<String>) {
    let file = ScannedFile {
        code: code.to_string(),
        module: Vec::new(),
        leaves: use_leaves(code),
    };
    let files = BTreeMap::from([(String::new(), file)]);
    let aliases = bindings(&files, name)
        .into_iter()
        .map(|(alias, _)| alias)
        .collect();
    (word_uses(code, name).len(), aliases)
}

/// Other names `item` is reachable by, with the file binding each: `use … as`,
/// `type … =` and re-exports, followed through the module each path names.
fn bindings(files: &BTreeMap<String, ScannedFile>, item: &str) -> Vec<(String, String)> {
    let type_alias = regex::Regex::new(r"\btype\s+(\w+)\b[^=;]*=([^;]*)").unwrap();
    let mut found: Vec<(String, String)> = Vec::new();
    loop {
        let before = found.len();
        for (relative, file) in files {
            let mut fresh = Vec::new();
            for leaf in &file.leaves {
                let (Some(last), Some(binds)) = (leaf.path.last(), &leaf.binds) else {
                    continue;
                };
                let from = || resolve_path(files, file, &leaf.path[..leaf.path.len() - 1]);
                let reaches = (last == item && binds != item)
                    || found.iter().any(|(name, at)| {
                        name == last && path_reaches(files, &from(), &files[at].module)
                    });
                if reaches && binds != "_" {
                    fresh.push(binds.clone());
                }
            }
            let local = found
                .iter()
                .filter(|(_, at)| at == relative)
                .map(|(name, _)| name.as_str())
                .chain([item]);
            for name in local.filter(|name| file.code.contains(*name) && file.code.contains("type"))
            {
                let named = |rhs: &str| !word_uses(rhs, name).is_empty();
                fresh.extend(
                    type_alias
                        .captures_iter(&file.code)
                        .filter(|c| named(&c[2]))
                        .map(|c| c[1].to_string()),
                );
            }
            for name in fresh {
                if !found.contains(&(name.clone(), relative.clone())) {
                    found.push((name, relative.clone()));
                }
            }
        }
        if found.len() == before {
            return found;
        }
    }
}

/// Files that may name a guarded item, each with the uses allowed there.
type Owners = &'static [(&'static str, usize)];

/// Guarded items reached outside their owners, or used past the budget of the file that
/// names them. An alias shares its item's owners and budgets.
fn guarded_item_violations(
    sources: &BTreeMap<String, String>,
    items: &[(&str, Owners)],
) -> Vec<String> {
    let mut violations = Vec::new();
    let files = scan_files(sources, &mut violations);
    let mut claimed: BTreeMap<(String, String), &str> = BTreeMap::new();
    for (needle, owners) in items {
        let aliases = bindings(&files, needle);
        for (name, at) in &aliases {
            let taken = claimed.insert((name.clone(), at.clone()), *needle);
            let reused = items.iter().any(|(other, ..)| *other == name.as_str());
            if reused || taken.is_some_and(|t| t != *needle) {
                violations.push(format!(
                    "{at}: alias {name} of {needle} reuses a guarded name"
                ));
            }
        }
        for (relative, (used, values)) in item_uses_by_file(&files, needle, &aliases) {
            let budget = owners.iter().find(|(owner, _)| *owner == relative);
            let Some((_, budget)) = budget else {
                violations.push(format!("{relative}: {needle}"));
                continue;
            };
            if *budget != usize::MAX && used > *budget {
                violations.push(format!("{relative}: {needle} used x{used} > {budget}"));
            } else if *budget != usize::MAX && values > 0 {
                // A budget counts calls; a function value could be called any number of times.
                violations.push(format!("{relative}: {needle} taken as a value x{values}"));
            }
        }
    }
    violations
}

/// Whether names bound in `module` are reachable through `path`: it names that module, names
/// no scanned module, or names one whose glob imports reach it.
fn path_reaches(files: &BTreeMap<String, ScannedFile>, path: &[String], module: &[String]) -> bool {
    path == module
        || files
            .values()
            .find(|other| other.module == path)
            .is_none_or(|other| other.sees_bare(files, false, module))
}

/// Uses of `needle` and its `aliases` per naming file, and how many take it as a value. An
/// alias path that cannot be ruled out counts.
fn item_uses_by_file(
    files: &BTreeMap<String, ScannedFile>,
    needle: &str,
    aliases: &[(String, String)],
) -> BTreeMap<String, (usize, usize)> {
    let word = regex::Regex::new(&format!(r"\b{}\b", regex::escape(needle))).unwrap();
    let mut uses = BTreeMap::new();
    for (relative, file) in files {
        let mut sites = word_sites(&file.code, needle);
        // A file names the item by a `use` or a use; a local binding of that name does not.
        let imported = || {
            use_spans(&file.code)
                .iter()
                .any(|at| word.is_match(&file.code[at.clone()]))
        };
        let mut named = !sites.is_empty() || (file.code.contains(needle) && imported());
        for (name, at) in aliases {
            let module = &files[at].module;
            let bare = file.sees_bare(files, at == relative, module);
            let before = sites.len();
            sites.extend(
                word_sites(&file.code, name)
                    .into_iter()
                    .filter(|(prefix, _)| {
                        let path = || resolve_path(files, file, prefix);
                        bare || (!prefix.is_empty() && path_reaches(files, &path(), module))
                    }),
            );
            named |= at == relative || sites.len() > before;
        }
        if named {
            let values = sites.iter().filter(|(_, called)| !called).count();
            uses.insert(relative.clone(), (sites.len(), values));
        }
    }
    uses
}

/// Files outside `owners` reaching the `Herdr` variant through `HostKind` or one of
/// its aliases: a variant or glob import, or a path to the variant.
fn herdr_variant_violations(sources: &BTreeMap<String, String>, owners: &[&str]) -> Vec<String> {
    let mut violations = Vec::new();
    let files = scan_files(sources, &mut violations);
    let kinds = bindings(&files, "HostKind");
    let variant = regex::Regex::new(r"\b(\w+)\s*::\s*Herdr\b").unwrap();
    for (relative, file) in files.iter().filter(|(r, _)| !owners.contains(&r.as_str())) {
        // Whether `path[k]`, reached through `path[..k]`, may name HostKind or an alias of it;
        // a qualifier that globs the alias's module, or names no scanned module, counts.
        let kind_at = |path: &[String], k: usize| {
            path[k] == "HostKind"
                || kinds.iter().any(|(name, at)| {
                    let module = &files[at].module;
                    *name == path[k]
                        && if k == 0 {
                            file.sees_bare(&files, at == relative, module)
                        } else {
                            path_reaches(&files, &resolve_path(&files, file, &path[..k]), module)
                        }
                })
        };
        let imported = file.leaves.iter().any(|leaf| {
            (0..leaf.path.len()).any(|k| {
                kind_at(&leaf.path, k)
                    && match leaf.path.get(k + 1) {
                        Some(next) => next == "Herdr",
                        None => leaf.binds.is_none(),
                    }
            })
        });
        let pathed = file.code.contains("Herdr")
            && variant.captures_iter(&file.code).any(|found| {
                let name = found.get(1).unwrap();
                let mut path = qualifier(&file.code, name.start());
                path.push(name.as_str().to_string());
                kind_at(&path, path.len() - 1)
            });
        if imported || pathed {
            violations.push(format!("{relative}: HostKind::Herdr import"));
        }
    }
    violations
}

const GUARD_ADAPTER: &str = "src/services/discord/inflight/host_recovery_guard.rs";
const SESSION_RECORD: &str = "src/services/session_host/session_record.rs";
/// The timeouts policy repair facade: the one production caller of the target guard.
const POLICY_REPAIR: &str = "src/engine/ops/timeouts_ops/host_repair.rs";

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
        ("src/services/session_host/herdr/provenance.rs", 0),
        // Dormant socket launch host: builds its own transports; nothing constructs it.
        ("src/services/session_host/herdr/launch_host.rs", 0),
        ("src/services/session_host/model.rs", 3),
        ("src/services/session_host/resolve.rs", 2),
        ("src/services/session_host/consumer_guard.rs", 2),
        ("src/services/session_host/legacy_collapse.rs", 1),
        ("src/services/session_host/tmux_host.rs", 0),
        ("src/services/session_host/process_host.rs", 0),
        ("src/services/discord/inflight/host_locator.rs", 1),
        ("src/services/provider/session_probe.rs", 2),
        (GUARD_ADAPTER, 1),
        (SESSION_RECORD, 1),
        (POLICY_REPAIR, 2),
        // Dormant Herdr launch: names the host for the pane location and its marker.
        ("src/services/herdr_launch.rs", 2),
        // Dormant restart reconcile: reads the marker beside the stored pane, never a host.
        (RECONCILE, 1),
        // Dormant source attach: takes the caller's reader, never constructs or routes to a host.
        ("src/services/discord/tui_prompt_relay/herdr_source.rs", 0),
        // Watcher host snapshot: reads the marker beside the admission map and row, never a host.
        (WATCH_HOST, 1),
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
        "SocketHerdrLaunchHost::new(",
    ];
    const LAUNCH_HOST: &str = "src/services/session_host/herdr/launch_host.rs";
    // Only the inflight binding CAS copies a locator and only the Claude launch writes a
    // (tmux) `.host_kind` marker. Termination holds a locator only as a target.
    const LOCATOR: &str = "src/services/discord/inflight/host_locator.rs";
    const MARKER: &str = "src/services/tmux_common/host_marker.rs";
    const INFLIGHT_MODEL: &str = "src/services/discord/inflight/model.rs";
    const CLEANUP_GATE: &str = "src/db/dispatched_sessions/hosted_execution.rs";
    const BINDING_CAS: &str =
        "src/services/discord/inflight/save_store/identity_gate/host_locator.rs";
    const IDENTITY_GATE: &str = "src/services/discord/inflight/save_store/identity_gate.rs";
    const CLAUDE_LAUNCH: &str = "src/services/claude/tui_session_launch.rs";
    const RESOLVE: &str = "src/services/session_host/resolve.rs";
    // Liveness consumers' local host reading: marker and row locator, never a Herdr route.
    const LIVENESS: &str = "src/services/discord/host_liveness.rs";
    // A Claude turn's own marker check before it probes, kills or launches by name.
    const CLAUDE_TURN_GATE: &str = "src/services/claude/host_gate.rs";
    const RECONCILE: &str = "src/services/discord/recovery_engine/host_reconcile.rs";
    const WATCH_HOST: &str = "src/services/discord/watchers/lifecycle/watch_host.rs";
    const READERS: &[(&str, &[&str])] = &[
        (
            "PersistedHostLocator",
            &[LOCATOR, INFLIGHT_MODEL, GUARD_ADAPTER, BINDING_CAS],
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
        (
            "HostKindMarker",
            &[
                MARKER,
                GUARD_ADAPTER,
                CLEANUP_GATE,
                RESOLVE,
                LIVENESS,
                CLAUDE_TURN_GATE,
                RECONCILE,
                WATCH_HOST,
            ],
        ),
        (
            "read_host_kind_marker",
            &[
                MARKER,
                CLEANUP_GATE,
                RESOLVE,
                GUARD_ADAPTER,
                LIVENESS,
                CLAUDE_TURN_GATE,
                RECONCILE,
                WATCH_HOST,
            ],
        ),
        (
            "host_marker::",
            &[
                GUARD_ADAPTER,
                CLEANUP_GATE,
                RESOLVE,
                CLAUDE_LAUNCH,
                LIVENESS,
                CLAUDE_TURN_GATE,
                RECONCILE,
                WATCH_HOST,
            ],
        ),
        ("record_tmux_host_marker", &[MARKER, CLAUDE_LAUNCH]),
        (".host_locator", &[GUARD_ADAPTER, BINDING_CAS, LIVENESS]),
        ("host_locator: Some", &[]),
        (
            "host_locator:",
            &[INFLIGHT_MODEL, GUARD_ADAPTER, IDENTITY_GATE, BINDING_CAS],
        ),
    ];
    let sources = production_sources();
    let owner_files: Vec<&str> = OWNERS.iter().map(|(owner, _)| *owner).collect();
    let mut violations = herdr_variant_violations(&sources, &owner_files);
    for (relative, prod) in &sources {
        let relative = relative.as_str();
        let owner = OWNERS.iter().find(|(owner, _)| *owner == relative);
        let named = match owner {
            Some(_) => Vec::new(),
            None => NEEDLES.iter().filter(|n| prod.contains(**n)).collect(),
        };
        let activated = ACTIVATIONS
            .iter()
            .filter(|n| prod.contains(**n))
            .filter(|n| {
                // The launch host owns its transports; the host itself must stay unconstructed.
                !(relative == LAUNCH_HOST && n.starts_with("HerdrSocketTransport::new("))
            });
        violations.extend(
            named
                .into_iter()
                .chain(activated)
                .map(|n| format!("{relative}: {n}")),
        );
        let code = code_tokens(prod);
        if !word_uses(&code, "herdr_pane").is_empty() {
            violations.push(format!("{relative}: herdr_pane use"));
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

// The session target resolver and consumer guard stay with their owners; production
// reaches them only through the keyed teardown gate and the policy repair facade.
#[test]
fn session_target_guard_stays_behind_the_keyed_gate() {
    const RESOLVE: &str = "src/services/session_host/resolve.rs";
    const GUARD: &str = "src/services/session_host/consumer_guard.rs";
    const ROOT: &str = "src/services/session_host.rs";
    const INPUT: &str = "src/services/claude_tui/host_input.rs";
    const ANY: usize = usize::MAX;
    // Needle, and each file that may name it with the calls allowed there beyond its `fn`.
    const ITEMS: &[(&str, Owners)] = &[
        (
            "resolve_session_target",
            &[
                (RESOLVE, 0),
                (ROOT, 0),
                (GUARD_ADAPTER, 1),
                (POLICY_REPAIR, 1),
            ],
        ),
        ("resolve_target_host", &[(RESOLVE, 1)]),
        ("legacy_target_host", &[(RESOLVE, 1)]),
        (
            "guard_first_state_change",
            &[(GUARD, 1), (ROOT, 0), (POLICY_REPAIR, 1)],
        ),
        (
            "clear_legacy_session",
            &[(GUARD, 0), (ROOT, 0), (GUARD_ADAPTER, 1)],
        ),
        (
            "probe_for_policy",
            &[(GUARD, 0), (ROOT, 0), (POLICY_REPAIR, 1)],
        ),
        (
            "legacy_ref",
            &[(RESOLVE, 0), (GUARD, 2), (POLICY_REPAIR, 1)],
        ),
        (
            "teardown_for_lookup",
            &[
                (GUARD_ADAPTER, 1),
                ("src/services/discord/inflight.rs", 0),
                ("src/services/discord/host_key_derivation.rs", 1),
                ("src/services/discord/host_defer_gate.rs", 1),
                ("src/services/discord/host_teardown_gate.rs", 1),
            ],
        ),
        ("with_inflight_row", &[(GUARD_ADAPTER, 1)]),
        ("locator_witness", &[(GUARD_ADAPTER, 1)]),
        ("marker_witness", &[(GUARD_ADAPTER, 1)]),
        (
            "session_record_witness",
            &[
                (SESSION_RECORD, 0),
                (ROOT, 0),
                (GUARD_ADAPTER, 1),
                (POLICY_REPAIR, 1),
            ],
        ),
        ("with_host_marker", &[(RESOLVE, 0), (POLICY_REPAIR, 1)]),
        (
            "ResolvedSessionTarget",
            &[
                (RESOLVE, ANY),
                (GUARD, ANY),
                (ROOT, ANY),
                (INPUT, ANY),
                (POLICY_REPAIR, ANY),
            ],
        ),
        ("from_session_target", &[(INPUT, 0)]),
        (
            "SessionTargetEvidence",
            &[
                (RESOLVE, ANY),
                (ROOT, ANY),
                (GUARD_ADAPTER, ANY),
                (POLICY_REPAIR, ANY),
            ],
        ),
        (
            "SessionTargetInput",
            &[
                (RESOLVE, ANY),
                (ROOT, ANY),
                (GUARD_ADAPTER, ANY),
                (POLICY_REPAIR, ANY),
            ],
        ),
        (
            "HostWitness",
            &[
                (RESOLVE, ANY),
                (ROOT, ANY),
                (GUARD_ADAPTER, ANY),
                (SESSION_RECORD, ANY),
                (POLICY_REPAIR, ANY),
            ],
        ),
        (
            "GuardVerdict",
            &[(GUARD, ANY), (ROOT, ANY), (POLICY_REPAIR, ANY)],
        ),
        ("PolicyProbe", &[(GUARD, ANY), (ROOT, ANY)]),
        ("consumer_guard", &[(ROOT, ANY)]),
        (
            "host_recovery_guard",
            &[("src/services/discord/inflight.rs", ANY)],
        ),
    ];
    let sources = production_sources();
    let violations = guarded_item_violations(&sources, ITEMS);
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

// The caller scans above run on today's tree, which has none of these shapes; this
// feeds each shape through the same scan so a regression in it cannot pass silently.
#[test]
fn caller_scan_follows_aliases_scopes_and_lexer_edges() {
    const RESOLVE: &str = "src/services/session_host/resolve.rs";
    const ROOT: &str = "src/services/session_host.rs";
    const CHILD: &str = "src/services/session_host/resolve/child.rs";
    const MODEL: &str = "src/services/session_host/model.rs";
    const OTHER: &str = "src/services/termination_audit.rs";
    const FACADE: &str = "src/services/facade.rs";
    const ITEMS: &[(&str, Owners)] = &[
        ("resolve_session_target", &[(RESOLVE, 0), (ROOT, 0)]),
        ("resolve_target_host", &[(RESOLVE, 1), (ROOT, 0)]),
        ("legacy_target_host", &[(RESOLVE, 1)]),
        ("HostWitness", &[(RESOLVE, usize::MAX), (ROOT, usize::MAX)]),
    ];
    const ALIAS: &str = "pub(crate) use self::resolve_session_target as target;\n";
    const ROOT_ALIAS: &str = "pub(crate) use resolve::resolve_session_target as target;\n";
    let scan = |extra: &[(&str, &str)]| {
        let mut sources: BTreeMap<String, String> = [
            (
                RESOLVE,
                "pub(crate) fn resolve_session_target() {}\n\
                 fn resolve_target_host() {}\nfn run() { resolve_target_host(); }\n",
            ),
            (
                ROOT,
                "mod resolve;\npub(crate) use resolve::resolve_session_target;\n",
            ),
            (MODEL, "pub(crate) enum HostKind { Tmux, Herdr }\n"),
            (OTHER, "fn ordinary() { let target = 7; let _ = target; }\n"),
        ]
        .into_iter()
        .map(|(file, text)| (file.to_string(), text.to_string()))
        .collect();
        for (file, text) in extra {
            sources.entry(file.to_string()).or_default().push_str(text);
        }
        let mut found = guarded_item_violations(&sources, ITEMS);
        found.extend(herdr_variant_violations(&sources, &[MODEL]));
        found
    };
    assert_eq!(scan(&[]), Vec::<String>::new());

    type Extra = &'static [(&'static str, &'static str)];
    let caught: &[(&str, Extra, &str)] = &[
        (
            "an alias shares its item's budget",
            &[(
                RESOLVE,
                "use self::resolve_target_host as more;\nfn f() { more(); }\n",
            )],
            "resolve.rs: resolve_target_host used x2 > 1",
        ),
        (
            "a budget holds for its own file only",
            &[(ROOT, "fn f() { resolve::resolve_target_host(); }\n")],
            "session_host.rs: resolve_target_host used x1 > 0",
        ),
        (
            "an alias may not reuse a guarded name",
            &[(
                RESOLVE,
                "use self::resolve_session_target as HostWitness;\nfn f() { let _r = HostWitness; }\n",
            )],
            "alias HostWitness of resolve_session_target reuses a guarded name",
        ),
        (
            "a nested block comment",
            &[(
                ROOT,
                "fn g(i: u8) {\n/* outer /* inner */ \" */\nlet _ = resolve_session_target(i);\nlet _ = \"done\";\n}\n",
            )],
            "session_host.rs: resolve_session_target used x1 > 0",
        ),
        (
            "a raw byte string",
            &[(
                ROOT,
                "fn g() {\nlet _ = br#\"inner \" quote\"#;\nlet _ = resolve_session_target();\nlet _ = \"done\";\n}\n",
            )],
            "session_host.rs: resolve_session_target used x1 > 0",
        ),
        (
            "an unclosed comment",
            &[(OTHER, "/* never closed\n")],
            "termination_audit.rs: unterminated comment or literal",
        ),
        (
            "an imported alias",
            &[
                (RESOLVE, ALIAS),
                (
                    OTHER,
                    "use crate::services::session_host::resolve::target;\nfn h() { target(); }\n",
                ),
            ],
            "termination_audit.rs: resolve_session_target",
        ),
        (
            "a qualified alias",
            &[(RESOLVE, ALIAS), (CHILD, "fn c() { super::target(); }\n")],
            "resolve/child.rs: resolve_session_target",
        ),
        (
            "a glob-imported alias",
            &[
                (RESOLVE, ALIAS),
                (CHILD, "use super::*;\nfn c() { target(); }\n"),
            ],
            "resolve/child.rs: resolve_session_target",
        ),
        (
            "a HostKind alias glob",
            &[(
                OTHER,
                "use crate::services::session_host::{host_for, HostKind as Kind6459};\n\
                 use Kind6459::*;\nfn route() { let _ = host_for(Herdr); }\n",
            )],
            "termination_audit.rs: HostKind::Herdr import",
        ),
        (
            "a HostKind alias group import",
            &[(
                OTHER,
                "use crate::services::session_host::HostKind as K;\nuse K::{Herdr};\n",
            )],
            "termination_audit.rs: HostKind::Herdr import",
        ),
        (
            "a HostKind alias path",
            &[(
                OTHER,
                "use crate::services::session_host::HostKind as K;\nfn f() { let _ = K :: Herdr; }\n",
            )],
            "termination_audit.rs: HostKind::Herdr import",
        ),
        (
            "a HostKind alias glob through a module alias",
            &[
                (ROOT, "pub(crate) use model::HostKind as Kind;\n"),
                (
                    OTHER,
                    "use crate::services::session_host as hosts;\nuse hosts::Kind::*;\n\
                     fn scan_gap() { let _ = hosts::host_for(Herdr); }\n",
                ),
            ],
            "termination_audit.rs: HostKind::Herdr import",
        ),
        (
            "a HostKind alias path through a glob re-export",
            &[
                (ROOT, "pub(crate) use model::HostKind as Kind;\n"),
                (FACADE, "pub(crate) use crate::services::session_host::*;\n"),
                (
                    OTHER,
                    "fn route() { let _ = crate::services::facade::Kind::Herdr; }\n",
                ),
            ],
            "termination_audit.rs: HostKind::Herdr import",
        ),
        (
            "a HostKind alias import through a glob re-export",
            &[
                (ROOT, "pub(crate) use model::HostKind as Kind;\n"),
                (FACADE, "pub(crate) use crate::services::session_host::*;\n"),
                (OTHER, "use crate::services::facade::Kind::Herdr;\n"),
            ],
            "termination_audit.rs: HostKind::Herdr import",
        ),
        (
            "an item alias through a module alias",
            &[
                (ROOT, ROOT_ALIAS),
                (
                    OTHER,
                    "use crate::services::session_host as facade;\nfn f() { facade::target(); }\n",
                ),
            ],
            "termination_audit.rs: resolve_session_target",
        ),
        (
            "an item alias through a re-exported module alias",
            &[
                (ROOT, ROOT_ALIAS),
                (
                    CHILD,
                    "pub(crate) use crate::services::session_host as facade;\n",
                ),
                (
                    OTHER,
                    "use crate::services::session_host::resolve::child::facade;\n\
                     fn f() { facade::target(); }\n",
                ),
            ],
            "termination_audit.rs: resolve_session_target",
        ),
        (
            "a shadow ends with its block",
            &[(
                ROOT,
                "pub(crate) use resolve::resolve_session_target as target;\n\
                 fn a() { let target = || (); target(); }\nfn b() { target(); }\n",
            )],
            "session_host.rs: resolve_session_target used x1 > 0",
        ),
        (
            "a budgeted function taken as a value",
            &[(
                RESOLVE,
                "fn f() { let g = legacy_target_host; g(); g(); }\n",
            )],
            "resolve.rs: legacy_target_host taken as a value x1",
        ),
        (
            "a let that reads the alias",
            &[
                (ROOT, "fn a() { let target = target(); }\n"),
                (ROOT, ROOT_ALIAS),
            ],
            "session_host.rs: resolve_session_target used x1 > 0",
        ),
    ];
    for (label, extra, expected) in caught {
        let found = scan(extra);
        assert!(
            found.iter().any(|v| v.contains(expected)),
            "{label}: expected {expected:?} in {found:?}"
        );
    }

    let clean: &[(&str, Extra)] = &[
        ("an alias stays in its own scope", &[(RESOLVE, ALIAS)]),
        (
            "a local closure shadows the alias",
            &[(
                ROOT,
                "pub(crate) use resolve::resolve_session_target as target;\n\
                 fn unrelated() { let target = || (); target(); }\n",
            )],
        ),
        (
            "a comment names the item",
            &[(OTHER, "// resolve_session_target is not used here.\n")],
        ),
        (
            "an unrelated variant or glob",
            &[(
                OTHER,
                "use crate::services::session_host::HostKind as K;\nuse std::collections::*;\n\
                 enum Other { Herdr }\nfn f() { let _ = (K::Tmux, Other::Herdr); }\n",
            )],
        ),
    ];
    for (label, extra) in clean {
        assert_eq!(scan(extra), Vec::<String>::new(), "{label}");
    }
}

// The name-only teardown entries run without a host guard. Each production call is
// listed with why it keeps the name; a new call, or a moved caller going back, fails.
#[test]
fn name_only_teardown_calls_stay_on_the_reviewed_list() {
    const SWEEP: &str = "a spawn-time sweep; each site's reason is checked below";
    const BEFORE_WRITER: &str = "behind the turn-key host check, before the writer posts the row";
    const UNKEYED: &str = "a turn with no session key keeps the main teardown";
    const OWNED: &str = "owned by another piece";
    const MISSING: &str = "a Missing row path keeps it name-only";
    const ENTRY: &str = "forwarded by the guarded entry";
    const CALLS: &[(&str, Listed)] = &[
        (
            "record_termination_for_tmux",
            &[
                ("src/services/termination_audit.rs", 1, ENTRY),
                ("src/engine/ops/exec_ops.rs", 1, MISSING),
                (
                    "src/services/discord/tmux_watcher/pre_emit_guard.rs",
                    1,
                    MISSING,
                ),
                (
                    "src/services/discord/tmux_watcher/post_stream_exit.rs",
                    1,
                    MISSING,
                ),
                (
                    "src/services/discord/tmux_watcher/terminal_abort_exits.rs",
                    1,
                    MISSING,
                ),
                (
                    "src/services/discord/watchers/lifecycle/restore.rs",
                    1,
                    MISSING,
                ),
                (
                    "src/services/discord/router/message_handler/provider_isolation.rs",
                    1,
                    BEFORE_WRITER,
                ),
                ("src/services/provider_teardown.rs", 1, UNKEYED),
                ("src/services/codex.rs", 1, OWNED),
                (
                    "src/services/provider/cancel_token_cleanup/executor.rs",
                    1,
                    OWNED,
                ),
                ("src/services/claude_tui/host_input.rs", 1, OWNED),
            ],
        ),
        (
            "cleanup_session_temp_files",
            &[
                ("src/services/tmux_common.rs", 1, ENTRY),
                ("src/services/claude/tui_session_launch.rs", 1, SWEEP),
                ("src/services/claude.rs", 1, SWEEP),
                ("src/services/codex.rs", 2, SWEEP),
                ("src/services/qwen/session_lifecycle.rs", 1, SWEEP),
                (
                    "src/services/discord/router/message_handler/provider_isolation.rs",
                    1,
                    BEFORE_WRITER,
                ),
                ("src/services/discord/tmux_reaper.rs", 2, MISSING),
                ("src/services/discord/commands/control.rs", 1, MISSING),
                ("src/services/turn_lifecycle.rs", 1, MISSING),
            ],
        ),
        (
            "reset_managed_process_session",
            &[
                ("src/services/discord/commands/control.rs", 2, MISSING),
                ("src/services/discord/commands/mod.rs", 0, ENTRY),
                ("src/services/discord/health/recovery.rs", 1, MISSING),
                ("src/services/discord/admin_host_guard.rs", 1, MISSING),
            ],
        ),
    ];
    // Keeping Herdr sessions out of these launch paths belongs to launch admission; none
    // of the reasons below claims more than its own function shows.
    const SWEEPS: &[(&str, &str, Sweep)] = &[
        (
            "src/services/claude.rs",
            "execute_streaming_local_tmux",
            Sweep::C1Gated,
        ),
        (
            "src/services/codex.rs",
            "execute_streaming_local_tmux",
            Sweep::C1Gated,
        ),
        (
            "src/services/codex.rs",
            "execute_streaming_local_tui_tmux",
            Sweep::ExistingTuiPath,
        ),
        (
            "src/services/claude/tui_session_launch.rs",
            "prepare_and_create_claude_tui_session",
            Sweep::ExistingTuiPath,
        ),
        (
            "src/services/qwen/session_lifecycle.rs",
            "execute_streaming_local_tmux",
            Sweep::ExistingQwenPath,
        ),
    ];
    let sources = production_sources();
    let mut violations = listed_call_violations(&sources, CALLS);
    violations.extend(sweep_site_violations(&sources, SWEEPS));
    assert!(
        violations.is_empty(),
        "name-only teardown calls: {violations:?}"
    );
}

/// Why a spawn-time sweep of a session's temp files keeps the name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sweep {
    /// A refused C1 host gate earlier in the function returns before the sweep.
    C1Gated,
    /// The existing D/P10 teardown of a TUI launch stays in front of it.
    ExistingTuiPath,
    /// The existing Qwen launch path, which no host gate covers yet.
    ExistingQwenPath,
}

/// Direct `cleanup_session_temp_files` calls per (file, function) against the sweep list;
/// each reason must hold in its function: a C1 gate before the call, or no C1 gate at all.
fn sweep_site_violations(
    sources: &BTreeMap<String, String>,
    sweeps: &[(&str, &str, Sweep)],
) -> Vec<String> {
    let call = regex::Regex::new(r"\bcleanup_session_temp_files\s*\(").unwrap();
    let mut found: BTreeMap<(String, String), Vec<usize>> = BTreeMap::new();
    let files: BTreeSet<&str> = sweeps.iter().map(|(file, ..)| *file).collect();
    let mut violations = Vec::new();
    for file in &files {
        let code = code_tokens(&sources[*file]);
        for at in call.find_iter(&code).map(|m| m.start()) {
            let Some(function) = enclosing_fn(&code, at) else {
                violations.push(format!("{file}: sweep outside any function"));
                continue;
            };
            found
                .entry((file.to_string(), function))
                .or_default()
                .push(at);
        }
    }
    for (file, function, why) in sweeps {
        let key = (file.to_string(), function.to_string());
        let Some(calls) = found.remove(&key) else {
            violations.push(format!("{file}#{function}: listed sweep not found"));
            continue;
        };
        let code = code_tokens(&sources[*file]);
        let body = fn_body(&code, &format!("fn {function}("));
        let gated = |at: usize| code[body.start..at].contains("teardown_tmux(");
        let holds = match why {
            Sweep::C1Gated => calls.iter().all(|at| gated(*at)),
            Sweep::ExistingTuiPath => {
                !file.contains("/qwen/") && !code[body.clone()].contains("teardown_tmux(")
            }
            Sweep::ExistingQwenPath => {
                file.contains("/qwen/") && !code[body.clone()].contains("teardown_tmux(")
            }
        };
        if calls.len() != 1 || !holds {
            violations.push(format!("{file}#{function}: x{} {why:?}", calls.len()));
        }
    }
    violations.extend(
        found
            .into_keys()
            .map(|(file, function)| format!("{file}#{function}: unlisted sweep")),
    );
    violations
}

/// Name of the innermost `fn` whose body holds byte `at` of token-only code.
fn enclosing_fn(code: &str, at: usize) -> Option<String> {
    let signature = regex::Regex::new(r"\bfn\s+(\w+)").unwrap();
    signature
        .captures_iter(code)
        .filter(|found| found.get(0).unwrap().start() < at)
        .filter_map(|found| {
            let open =
                found.get(0).unwrap().end() + code[found.get(0).unwrap().end()..].find('{')?;
            let mut depth = 0usize;
            let close = code[open..].char_indices().find_map(|(offset, ch)| {
                match ch {
                    '{' => depth += 1,
                    '}' => depth -= 1,
                    _ => {}
                }
                (depth == 0).then_some(open + offset)
            })?;
            (open < at && at < close).then(|| (open, found[1].to_string()))
        })
        .max_by_key(|(open, _)| *open)
        .map(|(_, name)| name)
}

/// (file, calls there, why the call keeps the name)
type Listed = &'static [(&'static str, usize, &'static str)];

/// Each needle's calls per file against its list: an unlisted file, another count, a
/// listed file with none, or the function taken as a value to call later all fail.
fn listed_call_violations(
    sources: &BTreeMap<String, String>,
    calls: &[(&str, Listed)],
) -> Vec<String> {
    let mut violations = Vec::new();
    let files = scan_files(sources, &mut violations);
    for (needle, listed) in calls {
        let uses = item_uses_by_file(&files, needle, &bindings(&files, needle));
        for (relative, (used, values)) in &uses {
            if *values > 0 {
                violations.push(format!("{relative}: {needle} taken as a value x{values}"));
            }
            match listed.iter().find(|(file, ..)| file == relative) {
                None => violations.push(format!("{relative}: unlisted {needle} x{used}")),
                Some((_, count, why)) if count != used => violations.push(format!(
                    "{relative}: {needle} x{used}, listed x{count} ({why})"
                )),
                Some(_) => {}
            }
        }
        violations.extend(
            listed
                .iter()
                .filter(|(file, ..)| !uses.contains_key(*file))
                .map(|(file, count, _)| format!("{file}: {needle} listed x{count}, not found")),
        );
    }
    violations
}

// The list scan above runs on today's tree; these shapes reach a listed cleanup by a
// fully qualified re-export or as a function value, and an unrelated closure does not.
#[test]
fn name_only_scan_follows_reexports_and_function_values() {
    const COMMON: &str = "src/services/tmux_common.rs";
    const FACADE: &str = "src/services/facade.rs";
    const CALLER: &str = "src/services/caller.rs";
    const OTHER: &str = "src/services/other.rs";
    const FACADE1: &str = "src/services/facade1.rs";
    const FACADE2: &str = "src/services/facade2.rs";
    const FACADE3: &str = "src/services/facade3.rs";
    const WIPE: (&str, &str) = (
        COMMON,
        "pub(crate) use self::cleanup_session_temp_files as wipe;\n",
    );
    const CALLS: &[(&str, Listed)] = &[(
        "cleanup_session_temp_files",
        &[(COMMON, 1, "the one reviewed call")],
    )];
    let scan = |extra: &[(&str, &str)]| {
        let mut sources: BTreeMap<String, String> = BTreeMap::from([(
            COMMON.to_string(),
            "pub(crate) fn cleanup_session_temp_files(n: &str) {}\n\
             fn reviewed(n: &str) { cleanup_session_temp_files(n); }\n"
                .to_string(),
        )]);
        for (file, text) in extra {
            sources.entry(file.to_string()).or_default().push_str(text);
        }
        listed_call_violations(&sources, CALLS)
    };
    assert_eq!(scan(&[]), Vec::<String>::new());

    type Extra = &'static [(&'static str, &'static str)];
    let caught: &[(&str, Extra, &str)] = &[
        (
            "a fully qualified path through a re-exported module",
            &[
                (
                    COMMON,
                    "pub(crate) use self::cleanup_session_temp_files as wipe;\n",
                ),
                (
                    FACADE,
                    "pub(crate) use crate::services::tmux_common as cleanup_mod;\n",
                ),
                (
                    CALLER,
                    "fn go(n: &str) { crate::services::facade::cleanup_mod::wipe(n); }\n",
                ),
            ],
            "caller.rs: unlisted cleanup_session_temp_files x1",
        ),
        (
            "a fully qualified path through a glob re-export",
            &[
                (
                    COMMON,
                    "pub(crate) use self::cleanup_session_temp_files as wipe;\n",
                ),
                (FACADE, "pub(crate) use crate::services::tmux_common::*;\n"),
                (
                    CALLER,
                    "fn go(n: &str) { crate::services::facade::wipe(n); }\n",
                ),
            ],
            "caller.rs: unlisted cleanup_session_temp_files x1",
        ),
        (
            "a fully qualified path through two glob re-exports",
            &[
                WIPE,
                (FACADE1, "pub(crate) use crate::services::tmux_common::*;\n"),
                (FACADE2, "pub(crate) use crate::services::facade1::*;\n"),
                (
                    CALLER,
                    "fn go(n: &str) { crate::services::facade2::wipe(n); }\n",
                ),
            ],
            "caller.rs: unlisted cleanup_session_temp_files x1",
        ),
        (
            "a fully qualified path through three glob re-exports",
            &[
                WIPE,
                (FACADE1, "pub(crate) use crate::services::tmux_common::*;\n"),
                (FACADE2, "pub(crate) use crate::services::facade1::*;\n"),
                (FACADE3, "pub(crate) use super::facade2::*;\n"),
                (
                    CALLER,
                    "fn go(n: &str) { crate::services::facade3::wipe(n); }\n",
                ),
            ],
            "caller.rs: unlisted cleanup_session_temp_files x1",
        ),
        (
            "a glob cycle with one way out to the alias",
            &[
                WIPE,
                (FACADE1, "pub(crate) use crate::services::facade2::*;\n"),
                (
                    FACADE2,
                    "pub(crate) use crate::services::facade1::*;\n\
                     pub(crate) use crate::services::tmux_common::*;\n",
                ),
                (
                    CALLER,
                    "fn go(n: &str) { crate::services::facade1::wipe(n); }\n",
                ),
            ],
            "caller.rs: unlisted cleanup_session_temp_files x1",
        ),
        (
            "a module alias re-exported through a glob",
            &[
                WIPE,
                (FACADE1, "pub(crate) use crate::services::tmux_common::*;\n"),
                (FACADE2, "pub(crate) use crate::services::facade1 as f1;\n"),
                (FACADE3, "pub(crate) use crate::services::facade2::*;\n"),
                (
                    CALLER,
                    "fn go(n: &str) { crate::services::facade3::f1::wipe(n); }\n",
                ),
            ],
            "caller.rs: unlisted cleanup_session_temp_files x1",
        ),
        (
            "a bare name through two glob imports",
            &[
                WIPE,
                (FACADE1, "pub(crate) use crate::services::tmux_common::*;\n"),
                (FACADE2, "pub(crate) use crate::services::facade1::*;\n"),
                (
                    CALLER,
                    "use crate::services::facade2::*;\nfn go(n: &str) { wipe(n); }\n",
                ),
            ],
            "caller.rs: unlisted cleanup_session_temp_files x1",
        ),
        (
            "a glob chain through a crate module no file answers for",
            &[
                WIPE,
                (FACADE1, "pub(crate) use crate::services::gone::*;\n"),
                (
                    CALLER,
                    "fn go(n: &str) { crate::services::facade1::wipe(n); }\n",
                ),
            ],
            "caller.rs: unlisted cleanup_session_temp_files x1",
        ),
        (
            "a glob-imported alias renamed again by another module",
            &[
                WIPE,
                (FACADE1, "pub(crate) use crate::services::tmux_common::*;\n"),
                (
                    FACADE2,
                    "pub(crate) use crate::services::facade1::wipe as erase;\n",
                ),
                (
                    CALLER,
                    "fn go(n: &str) { crate::services::facade2::erase(n); }\n",
                ),
            ],
            "caller.rs: unlisted cleanup_session_temp_files x1",
        ),
        (
            "a glob behind a module alias no file answers for",
            &[
                WIPE,
                (
                    CALLER,
                    "use crate::services::gone as g;\nuse g::*;\nfn go(n: &str) { wipe(n); }\n",
                ),
            ],
            "caller.rs: unlisted cleanup_session_temp_files x1",
        ),
        (
            "an alias path that names no scanned module",
            &[
                (
                    COMMON,
                    "pub(crate) use self::cleanup_session_temp_files as wipe;\n",
                ),
                (
                    CALLER,
                    "fn go(n: &str) { crate::services::gone::wipe(n); }\n",
                ),
            ],
            "caller.rs: unlisted cleanup_session_temp_files x1",
        ),
        (
            "the item rebound under its own name",
            &[(
                COMMON,
                "fn again(n: &str) {\n\
                 let cleanup_session_temp_files = crate::services::tmux_common::cleanup_session_temp_files;\n\
                 cleanup_session_temp_files(n);\ncleanup_session_temp_files(n);\n}\n",
            )],
            "tmux_common.rs: cleanup_session_temp_files taken as a value x1",
        ),
        (
            "a function pointer under another name",
            &[(
                CALLER,
                "fn go(n: &str) { let f = crate::services::tmux_common::cleanup_session_temp_files; f(n); }\n",
            )],
            "caller.rs: cleanup_session_temp_files taken as a value x1",
        ),
        (
            "a closure of the same name that calls the item",
            &[(
                COMMON,
                "fn again(n: &str) {\n\
                 let cleanup_session_temp_files = |n: &str| crate::services::tmux_common::cleanup_session_temp_files(n);\n\
                 cleanup_session_temp_files(n);\n}\n",
            )],
            "tmux_common.rs: cleanup_session_temp_files x3, listed x1",
        ),
    ];
    for (label, extra, expected) in caught {
        let found = scan(extra);
        assert!(
            found.iter().any(|v| v.contains(expected)),
            "{label}: expected {expected:?} in {found:?}"
        );
    }

    let clean: &[(&str, Extra)] = &[
        (
            "another module's function renamed after a glob of that module",
            &[
                WIPE,
                (OTHER, "pub(crate) fn wipe(n: &str) {}\n"),
                (FACADE1, "pub(crate) use crate::services::other::*;\n"),
                (
                    FACADE2,
                    "pub(crate) use crate::services::facade1::wipe as erase;\n",
                ),
                (
                    CALLER,
                    "fn go(n: &str) { crate::services::facade2::erase(n); }\n",
                ),
            ],
        ),
        (
            "an unrelated local closure",
            &[(
                CALLER,
                "fn go(n: &str) {\n\
                 let cleanup_session_temp_files = |_: &str| ();\n\
                 cleanup_session_temp_files(n);\n}\n",
            )],
        ),
        (
            "a glob cycle that never reaches the alias",
            &[
                WIPE,
                (FACADE1, "pub(crate) use crate::services::facade2::*;\n"),
                (FACADE2, "pub(crate) use crate::services::facade1::*;\n"),
                (
                    CALLER,
                    "fn go(n: &str) { crate::services::facade2::wipe(n); }\n",
                ),
            ],
        ),
        (
            "another module's function of the alias name through two glob re-exports",
            &[
                WIPE,
                (OTHER, "pub(crate) fn wipe(n: &str) {}\n"),
                (FACADE1, "pub(crate) use crate::services::other::*;\n"),
                (FACADE2, "pub(crate) use crate::services::facade1::*;\n"),
                (
                    CALLER,
                    "fn go(n: &str) { crate::services::facade2::wipe(n); }\n",
                ),
            ],
        ),
        (
            "another module's function of the alias name through a re-exported module",
            &[
                (
                    COMMON,
                    "pub(crate) use self::cleanup_session_temp_files as wipe;\n",
                ),
                (OTHER, "pub(crate) fn wipe(n: &str) {}\n"),
                (
                    FACADE,
                    "pub(crate) use crate::services::other as other_mod;\n",
                ),
                (
                    CALLER,
                    "fn go(n: &str) { crate::services::facade::other_mod::wipe(n); }\n",
                ),
            ],
        ),
    ];
    for (label, extra) in clean {
        assert_eq!(scan(extra), Vec::<String>::new(), "{label}");
    }
}

/// Byte range of the body of the first `signature` in token-only code.
fn fn_body(code: &str, signature: &str) -> std::ops::Range<usize> {
    let start = code.find(signature).expect(signature);
    let open = start + code[start..].find('{').expect(signature);
    let mut depth = 0usize;
    for (offset, ch) in code[open..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' if depth == 1 => return open..open + offset,
            '}' => depth -= 1,
            _ => {}
        }
    }
    open..code.len()
}

// Only the Claude turn gate names the typed probe entries outside the owner; inside it
// the observer runs only in the `for_target` body, which the owner never calls.
#[test]
fn typed_session_probe_entries_have_no_production_caller() {
    const OWNER: &str = "src/services/provider/session_probe.rs";
    const CONSUMER: &str = "src/services/claude/host_gate.rs";
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
    let mut violations: Vec<String> = sources
        .iter()
        .filter(|(relative, _)| ![OWNER, CONSUMER].contains(&relative.as_str()))
        .flat_map(|(relative, prod)| {
            ENTRIES
                .iter()
                .filter(|entry| prod.contains(**entry))
                .map(move |entry| format!("{relative}: {entry}"))
        })
        .collect();
    let code = code_tokens(owner);
    let dormant = fn_body(&code, "fn for_target(");
    let imports = use_spans(&code);
    let observer = regex::Regex::new(r"\bobserve_session_liveness\b").unwrap();
    for found in observer.find_iter(&code) {
        let defined = code[..found.start()].trim_end().ends_with("fn");
        let imported = imports.iter().any(|span| span.contains(&found.start()));
        if !defined && !imported && !dormant.contains(&found.start()) {
            violations.push(format!(
                "{OWNER}: observe_session_liveness outside for_target"
            ));
        }
    }
    for (entry, allowed) in [("for_target", 0), ("observe_session_liveness", 1)] {
        let (uses, aliases) = item_uses(&code, entry);
        if uses > allowed || !aliases.is_empty() {
            violations.push(format!("{OWNER}: {entry} x{uses} {aliases:?}"));
        }
    }
    assert!(
        violations.is_empty(),
        "typed probe production caller: {violations:?}"
    );
}

// Draft guard: the Claude warm follow-up reaches the pane only through the host
// input executor, and a fresh session only follows an executor retire.
#[test]
fn claude_warm_followup_reaches_tmux_only_through_the_executor() {
    const HOSTING: &str = "src/services/claude_tui/hosting/";
    const WARM: &str = "src/services/claude_tui/hosting/warm_followup.rs";
    const DIRECT: &[&str] = &[
        "tmux::",
        "kill_session",
        "send_keys",
        "capture_pane",
        "record_termination_for_tmux",
        "record_tmux_exit_reason",
    ];
    let sources = production_sources();
    let hosting: Vec<(&String, String)> = sources
        .iter()
        .filter(|(relative, _)| relative.starts_with(HOSTING))
        .map(|(relative, prod)| (relative, code_tokens(prod)))
        .collect();
    assert!(
        hosting.len() >= 4 && sources[WARM].contains("fn try_claude_tui_warm_followup("),
        "source scan must see the hosting files"
    );
    let mut violations = Vec::new();
    for (relative, code) in &hosting {
        violations.extend(
            DIRECT
                .iter()
                .filter(|direct| code.contains(**direct))
                .map(|direct| format!("{relative}: {direct}")),
        );
        let (fresh, aliases) = item_uses(code, "fresh_claude_tui_session_resolution");
        let allowed = usize::from(relative.as_str() == WARM);
        if fresh > allowed || !aliases.is_empty() {
            violations.push(format!("{relative}: fresh resolution x{fresh} {aliases:?}"));
        }
    }
    assert!(
        violations.is_empty(),
        "warm follow-up bypasses the executor: {violations:?}"
    );
}
