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
            format: None,
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
        format: None,
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

/// Top-level comma-separated arguments of a `cfg` list, skipping literals.
fn cfg_args(args: &str) -> Vec<String> {
    let chars: Vec<char> = args.chars().collect();
    let (mut parts, mut from, mut depth, mut k) = (Vec::new(), 0, 0usize, 0);
    while k < chars.len() {
        if let Some(end) = literal_end(&chars, k) {
            k = end;
            continue;
        }
        match chars[k] {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(chars[from..k].iter().collect());
                from = k + 1;
            }
            _ => {}
        }
        k += 1;
    }
    parts.push(chars[from..].iter().collect());
    parts
}

/// A `cfg` predicate that holds only in test builds: `test`, or `all(..)` with such an
/// argument. `any(..)` and `not(..)` can hold without `test`, so they stay production.
fn cfg_requires_test(predicate: &str) -> bool {
    let predicate = predicate.trim();
    let all = predicate
        .strip_prefix("all")
        .map(str::trim_start)
        .and_then(|rest| rest.strip_prefix('('))
        .and_then(|rest| rest.strip_suffix(')'));
    predicate == "test"
        || all.is_some_and(|args| cfg_args(args).iter().any(|a| cfg_requires_test(a)))
}

/// Index just past the `]` of the attribute opening at `i`; `None` if it never closes.
fn attribute_end(chars: &[char], i: usize) -> Option<usize> {
    let (mut k, mut brackets) = (i + 2, 1usize);
    while k < chars.len() {
        if let Some(end) = literal_end(chars, k) {
            k = end;
            continue;
        }
        match chars[k] {
            '[' => brackets += 1,
            ']' if brackets == 1 => return Some(k + 1),
            ']' => brackets -= 1,
            _ => {}
        }
        k += 1;
    }
    None
}

/// Index of the next token at or after `k`, past whitespace and comments.
fn skip_trivia(chars: &[char], mut k: usize) -> usize {
    while k < chars.len() {
        if chars[k].is_whitespace() {
            k += 1;
        } else if chars[k] == '/'
            && let Some(end) = literal_end(chars, k)
        {
            k = end;
        } else {
            break;
        }
    }
    k
}

/// Whether the item at `k` is a declaration or statement, which only `;` or its closing `}`
/// ends; fields, variants, arms and arguments also end at a top-level `,`.
fn item_head(chars: &[char], k: usize) -> bool {
    let word = |k: usize| {
        let k = skip_trivia(chars, k);
        let end = (k..chars.len())
            .find(|e| !(chars[*e].is_alphanumeric() || chars[*e] == '_'))
            .unwrap_or(chars.len());
        (chars[k..end].iter().collect::<String>(), end)
    };
    let (mut head, end) = word(k);
    if head == "pub" {
        let mut k = skip_trivia(chars, end);
        if chars.get(k) == Some(&'(') {
            k = (k..chars.len())
                .find(|e| chars[*e] == ')')
                .map_or(chars.len(), |e| e + 1);
        }
        head = word(k).0;
    }
    const HEADS: &[&str] = &[
        "fn", "mod", "impl", "trait", "struct", "enum", "const", "static", "type", "use", "extern",
        "async", "unsafe", "let",
    ];
    HEADS.contains(&head.as_str())
}

/// Source with every item whose attributes hold a `cfg` requiring `test` removed, plus the
/// `mod x;` files it gated: a `#[path]` value (relative to the file's directory) or the bare name.
fn production_text(text: &str) -> (String, Vec<(String, Option<String>)>) {
    let chars: Vec<char> = text.chars().collect();
    let (mut out, mut test_mods, mut i) = (String::new(), Vec::new(), 0);
    while i < chars.len() {
        if let Some(end) = literal_end(&chars, i) {
            out.extend(&chars[i..end.min(chars.len())]);
            i = end;
            continue;
        }
        if !chars[i..].starts_with(&['#', '[']) {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        // The whole run of outer attributes belongs to the item after it.
        let (mut run_end, mut gated) = (i, false);
        while chars[run_end..].starts_with(&['#', '['])
            && let Some(end) = attribute_end(&chars, run_end)
        {
            let attribute: String = chars[run_end + 2..end - 1].iter().collect();
            let predicate = attribute
                .trim()
                .strip_prefix("cfg")
                .map(str::trim_start)
                .and_then(|rest| rest.strip_prefix('('))
                .and_then(|rest| rest.trim_end().strip_suffix(')'));
            gated |= predicate.is_some_and(cfg_requires_test);
            run_end = end;
            let next = skip_trivia(&chars, end);
            if !chars[next..].starts_with(&['#', '[']) {
                break;
            }
            run_end = next;
        }
        if run_end == i {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        if !gated {
            out.extend(&chars[i..run_end]);
            i = run_end;
            continue;
        }
        // An item ends at `;` (or `,` unless it is a declaration) or its closing `}`;
        // an enclosing closer ends it unconsumed.
        let item = item_head(&chars, run_end);
        let (mut k, mut depth) = (run_end, 0usize);
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
                ';' if depth == 0 => {
                    let head: String = chars[i..k].iter().collect();
                    let words: Vec<&str> = head.split_whitespace().collect();
                    if let [.., "mod", name] = words.as_slice() {
                        let path = head
                            .split_once("#[path = \"")
                            .and_then(|(_, rest)| rest.split_once('"'));
                        test_mods.push((name.to_string(), path.map(|(file, _)| file.to_string())));
                    }
                    k += 1;
                    break;
                }
                ',' if depth == 0 && !item => {
                    k += 1;
                    break;
                }
                _ => {}
            }
            k += 1;
        }
        i = k;
    }
    (out, test_mods)
}

type ExternalModules = Vec<(String, Option<String>)>;

/// Top-level paths are exact; nested declarations also protect uncertain production layouts.
fn external_module_declarations(text: &str) -> (ExternalModules, ExternalModules) {
    static PATH_ATTR: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r#"^\s*path\s*=\s*"([^"]+)"\s*$"#).unwrap());
    let chars: Vec<char> = text.chars().collect();
    let (mut modules, mut nested, mut path, mut depth, mut i) =
        (Vec::new(), Vec::new(), None, 0usize, 0);
    while i < chars.len() {
        if let Some(end) = literal_end(&chars, i) {
            i = end;
        } else if chars[i..].starts_with(&['#', '[']) {
            let (mut k, mut brackets) = (i + 2, 1usize);
            while k < chars.len() && brackets > 0 {
                if let Some(end) = literal_end(&chars, k) {
                    k = end;
                    continue;
                }
                match chars[k] {
                    '[' => brackets += 1,
                    ']' => brackets -= 1,
                    _ => {}
                }
                k += 1;
            }
            if brackets == 0 {
                let attribute: String = chars[i + 2..k - 1].iter().collect();
                if let Some(found) = PATH_ATTR.captures(&attribute) {
                    path = Some(found[1].to_string());
                }
            }
            i = k;
        } else if chars[i].is_ascii_alphabetic() || chars[i] == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            if word == "mod" {
                while chars.get(i).is_some_and(|c| c.is_whitespace()) {
                    i += 1;
                }
                let start = i;
                while chars
                    .get(i)
                    .is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_')
                {
                    i += 1;
                }
                let name: String = chars[start..i].iter().collect();
                while chars.get(i).is_some_and(|c| c.is_whitespace()) {
                    i += 1;
                }
                if !name.is_empty() && chars.get(i) == Some(&';') {
                    if depth == 0 {
                        modules.push((name, path.take()));
                    } else {
                        nested.push((name, path.take()));
                    }
                }
                path = None;
            } else if matches!(
                word.as_str(),
                "fn" | "struct"
                    | "enum"
                    | "use"
                    | "impl"
                    | "trait"
                    | "type"
                    | "const"
                    | "static"
                    | "extern"
            ) {
                path = None;
            }
        } else {
            match chars[i] {
                '{' => {
                    depth += 1;
                    path = None;
                }
                '(' | '[' => depth += 1,
                '}' | ')' | ']' => depth = depth.saturating_sub(1),
                ';' => path = None,
                _ => {}
            }
            i += 1;
        }
    }
    (modules, nested)
}

fn external_modules(text: &str) -> ExternalModules {
    external_module_declarations(text).0
}

fn module_files(path: &Path, modules: ExternalModules) -> Vec<std::path::PathBuf> {
    let base = match path.file_stem().and_then(|stem| stem.to_str()) {
        Some("mod" | "lib" | "main") => path.parent().unwrap().to_path_buf(),
        _ => path.with_extension(""),
    };
    modules
        .into_iter()
        .flat_map(|(name, file)| match file {
            Some(file) => vec![path.parent().unwrap().join(file)],
            None => vec![
                base.join(format!("{name}.rs")),
                base.join(name).join("mod.rs"),
            ],
        })
        .map(|path| {
            let mut normalized = std::path::PathBuf::new();
            for component in path.components() {
                match component {
                    std::path::Component::ParentDir => {
                        normalized.pop();
                    }
                    std::path::Component::CurDir => {}
                    component => normalized.push(component.as_os_str()),
                }
            }
            normalized
        })
        .collect()
}

/// Test ancestry cannot hide a child also reached by an ungated production parent.
fn test_only_files(files: &BTreeMap<std::path::PathBuf, String>) -> BTreeSet<std::path::PathBuf> {
    let mut candidates = BTreeSet::new();
    let mut production = BTreeMap::new();
    for (path, text) in files {
        let (prod, tests) = production_text(text);
        candidates.extend(module_files(path, tests));
        production.insert(path.clone(), module_files(path, external_modules(&prod)));
    }
    let mut queue: Vec<_> = candidates.iter().cloned().collect();
    while let Some(path) = queue.pop() {
        if let Some(text) = files.get(&path) {
            for child in module_files(&path, external_modules(text)) {
                if candidates.insert(child.clone()) {
                    queue.push(child);
                }
            }
        }
    }
    for (path, text) in files {
        let (prod, _) = production_text(text);
        let nested = external_module_declarations(&prod).1;
        for (name, file) in nested {
            // An inline module's directory may be overridden; never hide a possible child.
            production.get_mut(path).unwrap().extend(
                candidates
                    .iter()
                    .filter(|child| {
                        if let Some(file) = &file {
                            let file = Path::new(file);
                            child.file_name() == file.file_name()
                        } else {
                            child.file_stem().is_some_and(|stem| stem == name.as_str())
                                || (child.file_name().is_some_and(|file| file == "mod.rs")
                                    && child
                                        .parent()
                                        .and_then(Path::file_name)
                                        .is_some_and(|stem| stem == name.as_str()))
                        }
                    })
                    .cloned(),
            );
        }
    }
    let mut live: BTreeSet<_> = files
        .keys()
        .filter(|path| !candidates.contains(*path))
        .cloned()
        .collect();
    let mut queue: Vec<_> = live.iter().cloned().collect();
    while let Some(path) = queue.pop() {
        for child in production.get(&path).into_iter().flatten() {
            if live.insert(child.clone()) {
                queue.push(child.clone());
            }
        }
    }
    candidates.retain(|path| !live.contains(path));
    candidates
}

/// Production text of every non-test `src/**/*.rs`, keyed by repo-relative path.
fn production_sources() -> BTreeMap<String, String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut stack = vec![root.join("src")];
    let mut files = BTreeMap::new();
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
            files.insert(path, text);
        }
    }
    let test_files = test_only_files(&files);
    let (mut total, mut kept, mut sources) = (0, 0, BTreeMap::new());
    for (path, text) in files {
        if test_files.contains(&path) {
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        let (prod, _) = production_text(&text);
        (total, kept) = (total + text.len(), kept + prod.len());
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
        let uses = item_uses_by_file(&files, needle, &aliases);
        for (owner, budget) in *owners {
            if *budget > 0 && *budget != usize::MAX {
                assert!(
                    sources.contains_key(*owner) && uses.get(*owner).is_some_and(|(n, _)| *n > 0),
                    "source scan missed {needle} caller {owner}"
                );
            }
        }
        for (relative, (used, values)) in uses {
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

const GUARD_ADAPTER: &str = "src/services/discord/inflight/host_recovery_guard.rs";
const UNBOUND_OBSERVER: &str = "src/services/discord/turn_presence/activity/unbound.rs";
const SESSION_RECORD: &str = "src/services/session_host/session_record.rs";
/// The timeouts policy repair facade: the one production caller of the target guard.
const POLICY_REPAIR: &str = "src/engine/ops/timeouts_ops/host_repair.rs";
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
                (UNBOUND_OBSERVER, 1),
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
        (
            "with_inflight_row",
            &[
                (GUARD_ADAPTER, 1),
                ("src/services/discord/inflight.rs", 0),
                (UNBOUND_OBSERVER, 1),
            ],
        ),
        ("locator_witness", &[(GUARD_ADAPTER, 1)]),
        (
            "marker_witness",
            &[
                (GUARD_ADAPTER, 1),
                ("src/services/discord/inflight.rs", 0),
                (UNBOUND_OBSERVER, 1),
            ],
        ),
        (
            "session_record_witness",
            &[
                (SESSION_RECORD, 0),
                (ROOT, 0),
                (GUARD_ADAPTER, 1),
                (POLICY_REPAIR, 1),
                (UNBOUND_OBSERVER, 1),
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
                (UNBOUND_OBSERVER, ANY),
            ],
        ),
        (
            "SessionTargetInput",
            &[
                (RESOLVE, ANY),
                (ROOT, ANY),
                (GUARD_ADAPTER, ANY),
                (POLICY_REPAIR, ANY),
                (UNBOUND_OBSERVER, ANY),
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
                (UNBOUND_OBSERVER, ANY),
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

/// Herdr content from an owner on another host, or from a thread without the owner holding the
/// pane, is refused before any call; the holding owner's own write still goes out once.
#[test]
fn herdr_content_fails_closed_outside_the_owner_that_holds_the_pane() {
    use crate::services::claude_tui::owner_input::with_scope;
    // A pane no other test's owner holds, so parallel owners never share this lease.
    let own = || HostSessionRef::herdr_pane("a4b-own");
    let scope = |host: &str, run: &mut dyn FnMut()| {
        let target =
            json!({"host": host, "owner": "a4b", "root": "/a4b", "execution_nonce": "a4b"});
        with_scope(
            "claude".into(),
            target,
            742703,
            std::sync::Arc::new(|_| Ok(())),
            std::sync::Arc::new(|_, _| Ok(())),
            std::sync::Arc::new(|_| Ok(())),
            run,
        )
    };
    let fenced = || {
        Ok(HostMutation::Refused(HostRefusal::Precondition(
            "owner_content_fence".into(),
        )))
    };
    let silent = host(Vec::new());
    scope("tmux", &mut || {
        assert_eq!(silent.send_text(own(), "x"), fenced());
        assert_eq!(silent.send_paste(own(), "x"), fenced());
        assert_eq!(silent.send_line(own(), "x"), fenced());
    });
    assert!(calls(&silent).is_empty());

    let own_call = HerdrRequest::PaneSendText {
        pane_id: "a4b-own".into(),
        text: "own".into(),
    };
    let herdr = std::sync::Arc::new(host(vec![(own_call, ok())]));
    let other = herdr.clone();
    scope("herdr", &mut || {
        assert_eq!(herdr.send_text(own(), "own"), Ok(HostMutation::Confirmed));
        let other = other.clone();
        let spawned = std::thread::spawn(move || {
            [
                other.send_text(own(), "x"),
                other.send_paste(own(), "x"),
                other.send_line(own(), "x"),
            ]
        });
        for refused in spawned.join().unwrap() {
            assert_eq!(refused, fenced());
        }
    });
    assert_eq!(calls(&herdr).len(), 1);
}
