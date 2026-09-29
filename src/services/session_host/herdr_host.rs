#![cfg_attr(not(test), allow(dead_code))]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use super::herdr::contract::{self, HerdrTransport};
use super::herdr::model::{
    ControlPlane, ENDPOINT_MISSING, HerdrCall, HerdrEndpoint, HerdrObservation, HerdrRequest,
};
use super::model::{
    HostCapabilities, HostError, HostKind, HostLiveness, HostMutation, HostPresence, HostRefusal,
    HostSessionRef,
};
use super::traits::InteractiveSessionHost;

/// Herdr pane host over an injected transport. It exists only with a
/// validated endpoint; nothing in production constructs one yet.
pub(crate) struct HerdrHost<T: HerdrTransport> {
    endpoint: HerdrEndpoint,
    transport: T,
    next_id: AtomicU64,
}

fn pane_id(session: HostSessionRef<'_>) -> Result<&str, HostError> {
    if session.kind != HostKind::Herdr || session.name.trim().is_empty() {
        return Err(HostError::Unsupported(session.kind, "herdr_pane_target"));
    }
    Ok(session.name)
}

fn refused(op: &'static str) -> Result<HostMutation, HostError> {
    Ok(HostMutation::Refused(HostRefusal::Unsupported {
        kind: HostKind::Herdr,
        op,
    }))
}

impl<T: HerdrTransport> HerdrHost<T> {
    pub(crate) fn new(endpoint: HerdrEndpoint, transport: T) -> Self {
        Self {
            endpoint,
            transport,
            next_id: AtomicU64::new(1),
        }
    }

    fn call(&self, request: HerdrRequest) -> (HerdrCall, contract::HerdrOutcome) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let call = HerdrCall {
            id: format!("adk-{id}"),
            request,
        };
        let outcome = self.transport.call(&call);
        (call, outcome)
    }

    fn exchange<R>(
        &self,
        session: HostSessionRef<'_>,
        request: impl FnOnce(String) -> HerdrRequest,
        adapt: fn(&HerdrCall, contract::HerdrOutcome, &str) -> Result<R, HostError>,
    ) -> Result<R, HostError> {
        let pane = pane_id(session)?;
        let (call, outcome) = self.call(request(pane.to_string()));
        adapt(&call, outcome, pane)
    }

    fn observe_pane(&self, session: HostSessionRef<'_>) -> HerdrObservation {
        let Ok(pane) = pane_id(session) else {
            return HerdrObservation::failed(ControlPlane::Reachable);
        };
        let (call, outcome) = self.call(HerdrRequest::SessionSnapshot {});
        contract::snapshot_observation(&call, outcome, pane)
    }

    pub(crate) fn observe(&self, session: HostSessionRef<'_>) -> HerdrObservation {
        let observation = self.observe_pane(session);
        let (Ok(pane), HostPresence::Present) = (pane_id(session), observation.presence()) else {
            return observation;
        };
        let (call, outcome) = self.call(HerdrRequest::PaneProcessInfo {
            pane_id: pane.to_string(),
        });
        contract::with_process_info(observation, &call, outcome, pane)
    }
}

impl<T: HerdrTransport> InteractiveSessionHost for HerdrHost<T> {
    fn kind(&self) -> HostKind {
        HostKind::Herdr
    }

    // Key grammar is unverified, so keys and interrupt stay refused.
    fn capabilities(&self) -> HostCapabilities {
        HostCapabilities {
            send_text: true,
            capture_screen: true,
            current_working_dir: true,
            execution_pid: true,
            ..HostCapabilities::default()
        }
    }

    fn presence(&self, session: HostSessionRef<'_>) -> HostPresence {
        self.observe_pane(session).presence()
    }

    fn liveness(&self, session: HostSessionRef<'_>) -> HostLiveness {
        self.observe(session).liveness()
    }

    fn send_text(
        &self,
        session: HostSessionRef<'_>,
        text: &str,
    ) -> Result<HostMutation, HostError> {
        let text = text.to_string();
        let request = |pane_id| HerdrRequest::PaneSendText { pane_id, text };
        self.exchange(session, request, contract::mutation_result)
    }

    fn send_keys(
        &self,
        _session: HostSessionRef<'_>,
        _keys: &[&str],
    ) -> Result<HostMutation, HostError> {
        refused("send_keys")
    }

    fn interrupt(&self, _session: HostSessionRef<'_>) -> Result<HostMutation, HostError> {
        refused("interrupt")
    }

    fn capture_screen(
        &self,
        session: HostSessionRef<'_>,
        scroll_back: i32,
    ) -> Result<String, HostError> {
        let request = |pane_id: String| contract::capture_request(&pane_id, scroll_back);
        self.exchange(session, request, contract::capture_result)
    }

    fn current_working_dir(
        &self,
        session: HostSessionRef<'_>,
    ) -> Result<Option<PathBuf>, HostError> {
        let request = |pane_id| HerdrRequest::PaneGet { pane_id };
        self.exchange(session, request, contract::working_dir_result)
    }

    fn execution_pid(&self, session: HostSessionRef<'_>) -> Result<Option<u32>, HostError> {
        let request = |pane_id| HerdrRequest::PaneProcessInfo { pane_id };
        self.exchange(session, request, contract::execution_pid_result)
    }
}

/// What `host_for(Herdr)` returns: no endpoint, so every call fails without I/O.
pub(crate) struct UnconfiguredHerdrHost;

fn no_endpoint<R>() -> Result<R, HostError> {
    Err(HostError::Unsupported(HostKind::Herdr, ENDPOINT_MISSING))
}

impl InteractiveSessionHost for UnconfiguredHerdrHost {
    fn kind(&self) -> HostKind {
        HostKind::Herdr
    }

    fn capabilities(&self) -> HostCapabilities {
        HostCapabilities::default()
    }

    fn presence(&self, _session: HostSessionRef<'_>) -> HostPresence {
        HostPresence::ProbeFailed
    }

    fn liveness(&self, _session: HostSessionRef<'_>) -> HostLiveness {
        HostLiveness::ProbeError
    }

    fn send_text(
        &self,
        _session: HostSessionRef<'_>,
        _text: &str,
    ) -> Result<HostMutation, HostError> {
        no_endpoint()
    }

    fn send_keys(
        &self,
        _session: HostSessionRef<'_>,
        _keys: &[&str],
    ) -> Result<HostMutation, HostError> {
        no_endpoint()
    }

    fn interrupt(&self, _session: HostSessionRef<'_>) -> Result<HostMutation, HostError> {
        no_endpoint()
    }

    fn capture_screen(
        &self,
        _session: HostSessionRef<'_>,
        _scroll_back: i32,
    ) -> Result<String, HostError> {
        no_endpoint()
    }

    fn current_working_dir(
        &self,
        _session: HostSessionRef<'_>,
    ) -> Result<Option<PathBuf>, HostError> {
        no_endpoint()
    }

    fn execution_pid(&self, _session: HostSessionRef<'_>) -> Result<Option<u32>, HostError> {
        no_endpoint()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::path::Path;
    use std::sync::Mutex;

    use serde_json::{Value, json};

    use super::*;
    use crate::services::session_host::herdr::contract::{CAPTURE_MAX_LINES, HerdrTransportError};
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

    #[derive(Default)]
    struct FakeTransport {
        script: Mutex<VecDeque<Scripted>>,
        calls: Mutex<Vec<HerdrCall>>,
    }

    impl HerdrTransport for FakeTransport {
        fn call(&self, call: &HerdrCall) -> contract::HerdrOutcome {
            self.calls.lock().unwrap().push(call.clone());
            let next = self.script.lock().unwrap().pop_front();
            let body = match next.expect("fake transport called more often than scripted") {
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

    fn host(script: Vec<Scripted>) -> HerdrHost<FakeTransport> {
        let endpoint = HerdrEndpoint::new("mac-mini", "pilot", Path::new("/tmp/h.sock"), "adk")
            .expect("valid endpoint");
        let transport = FakeTransport {
            script: Mutex::new(script.into()),
            ..FakeTransport::default()
        };
        HerdrHost::new(endpoint, transport)
    }

    fn calls(host: &HerdrHost<FakeTransport>) -> Vec<HerdrCall> {
        host.transport.calls.lock().unwrap().clone()
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
        let present = host(vec![snapshot(22, &["w1-0", PANE])]);
        assert_eq!(present.presence(pane()), HostPresence::Present);
        let missing = host(vec![snapshot(22, &["w1-0"]), snapshot(22, &["w1-0"])]);
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
            let probe = host(vec![failure()]);
            assert_eq!(
                probe.presence(pane()),
                HostPresence::ProbeFailed,
                "ProbeFailed must never read as Missing"
            );
            let probe = host(vec![failure()]);
            assert_eq!(
                probe.liveness(pane()),
                HostLiveness::ProbeError,
                "a failed probe must never read as dead"
            );
        }
    }

    #[test]
    fn herdr_liveness_needs_a_root_shell_pid() {
        let live = host(vec![snapshot(22, &[PANE]), process_info(PANE, json!(4242))]);
        let observation = live.observe(pane());
        assert_eq!(observation.execution, ExecutionState::Live);
        assert_eq!(
            (observation.revision, observation.shell_pid),
            (Some(7), Some(4242))
        );
        assert_eq!(observation.liveness(), HostLiveness::Live);
        let unknown = host(vec![snapshot(22, &[PANE]), process_info(PANE, Value::Null)]);
        assert_eq!(
            unknown.liveness(pane()),
            HostLiveness::ProbeError,
            "shell_pid null is not death"
        );
        let broken = host(vec![snapshot(22, &[PANE]), not_sent()]);
        assert_eq!(broken.liveness(pane()), HostLiveness::ProbeError);
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
        let herdr = host(vec![process_info(PANE, json!(4242))]);
        assert_eq!(
            herdr.execution_pid(pane()),
            Ok(Some(4242)),
            "execution_pid must map process_info.shell_pid"
        );
        let other_pane = host(vec![process_info("w9-9", json!(4242))]);
        assert!(matches!(
            other_pane.execution_pid(pane()),
            Err(HostError::Protocol(_))
        ));
    }

    #[test]
    fn herdr_working_dir_prefers_foreground_cwd() {
        let herdr = host(vec![Scripted::Result(
            json!({"type": "pane_info", "pane": pane_info(PANE)}),
        )]);
        assert_eq!(
            herdr.current_working_dir(pane()),
            Ok(Some(PathBuf::from("/work/sub")))
        );
        assert_eq!(
            calls(&herdr)[0].request,
            HerdrRequest::PaneGet {
                pane_id: PANE.into()
            }
        );
    }

    #[test]
    fn herdr_capture_maps_scroll_back_and_rejects_truncation() {
        let herdr = host(vec![
            read("visible", false),
            read("recent_unwrapped", false),
        ]);
        assert_eq!(herdr.capture_screen(pane(), 0), Ok("screen".to_string()));
        assert_eq!(herdr.capture_screen(pane(), -50), Ok("screen".to_string()));
        let sent: Vec<_> = calls(&herdr).into_iter().map(|call| call.request).collect();
        assert_eq!(sent[0], contract::capture_request(PANE, 0));
        assert!(matches!(
            &sent[1],
            HerdrRequest::PaneRead {
                source: HerdrReadSource::RecentUnwrapped,
                lines: Some(50),
                ..
            }
        ));
        assert!(matches!(
            contract::capture_request(PANE, i32::MIN),
            HerdrRequest::PaneRead {
                lines: Some(CAPTURE_MAX_LINES),
                ..
            }
        ));
        let truncated = host(vec![read("visible", true)]);
        assert!(matches!(
            truncated.capture_screen(pane(), 0),
            Err(HostError::Protocol(_))
        ));
        let wrong_source = host(vec![read("recent", false)]);
        assert!(matches!(
            wrong_source.capture_screen(pane(), 0),
            Err(HostError::Protocol(_))
        ));
    }

    #[test]
    fn herdr_send_text_keeps_ambiguous_outcomes_indeterminate() {
        let herdr = host(vec![ok()]);
        assert_eq!(
            herdr.send_text(pane(), "안녕\n"),
            Ok(HostMutation::Confirmed)
        );
        assert_eq!(
            calls(&herdr)[0].request,
            HerdrRequest::PaneSendText {
                pane_id: PANE.into(),
                text: "안녕\n".into()
            }
        );
        let refused = host(vec![not_sent()]);
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
            let herdr = host(vec![outcome()]);
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
            wire(HerdrRequest::SessionSnapshot {}),
            json!({"id": "adk-1", "method": "session.snapshot", "params": {}})
        );
        assert_eq!(
            wire(HerdrRequest::PaneProcessInfo {
                pane_id: PANE.into()
            }),
            json!({"id": "adk-1", "method": "pane.process_info", "params": {"pane_id": PANE}})
        );
        assert_eq!(
            wire(contract::capture_request(PANE, -5)),
            json!({"id": "adk-1", "method": "pane.read", "params": {
                "pane_id": PANE, "source": "recent_unwrapped", "lines": 5, "strip_ansi": true
            }})
        );
        assert_eq!(
            wire(HerdrRequest::PaneSendText {
                pane_id: PANE.into(),
                text: "hi".into()
            }),
            json!({"id": "adk-1", "method": "pane.send_text",
                "params": {"pane_id": PANE, "text": "hi"}})
        );
    }

    #[test]
    fn herdr_reply_needs_exactly_one_of_result_or_error() {
        let parse = |body| serde_json::from_value::<HerdrReply>(body);
        assert!(parse(json!({"id": "a"})).is_err());
        assert!(
            parse(
                json!({"id": "a", "result": {"type": "ok"}, "error": {"code": "c", "message": "m"}})
            )
            .is_err()
        );
        assert!(parse(json!({"id": "a", "result": {"type": "session_snapshot"}})).is_err());
        let other = parse(json!({"id": "a", "result": {"type": "pong", "version": "v"}}));
        assert!(matches!(
            other,
            Ok(HerdrReply {
                body: Ok(HerdrResult::Other),
                ..
            })
        ));
    }

    // Dormant guard: nothing outside the session_host module names a Herdr item.
    #[test]
    fn herdr_items_have_no_production_caller() {
        const OWNERS: &[&str] = &[
            "src/services/session_host.rs",
            "src/services/session_host/herdr_host.rs",
            "src/services/session_host/herdr/model.rs",
            "src/services/session_host/herdr/contract.rs",
            "src/services/session_host/model.rs",
            "src/services/session_host/resolve.rs",
            "src/services/session_host/legacy_collapse.rs",
            "src/services/session_host/tmux_host.rs",
            "src/services/session_host/process_host.rs",
        ];
        const NEEDLES: &[&str] = &[
            "HerdrHost",
            "HerdrEndpoint",
            "HerdrTransport",
            "HostKind::Herdr",
            "herdr_pane(",
            "session_host::herdr",
        ];
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut stack = vec![root.join("src")];
        let (mut scanned, mut violations) = (0, Vec::new());
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                let relative = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                if !relative.ends_with(".rs") || OWNERS.contains(&relative.as_str()) {
                    continue;
                }
                scanned += 1;
                let text = std::fs::read_to_string(&path).unwrap_or_default();
                violations.extend(
                    NEEDLES
                        .iter()
                        .filter(|n| text.contains(**n))
                        .map(|n| format!("{relative}: {n}")),
                );
            }
        }
        assert!(scanned > 100, "source scan found only {scanned} files");
        assert!(
            violations.is_empty(),
            "Herdr production caller: {violations:?}"
        );
    }
}
