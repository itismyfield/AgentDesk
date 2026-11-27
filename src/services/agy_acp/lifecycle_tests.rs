use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc::channel;
use std::time::{Duration, Instant};

#[cfg(test)]
use super::actor::{self, Event};
#[cfg(test)]
use super::actor::{Deadlines, Handle, TurnRequest};
#[cfg(test)]
use super::rpc::Inbound;
use super::*;
use crate::services::process::{ProcessGroupProbe, ProcessIdentity, process_group_probe};
use crate::services::provider::CancelToken;
#[cfg(test)]
use crate::services::stream_json_cli::codec::{AgyCodec, StreamJsonCodec};
use crate::test_support::wait::WAIT_CAP;
#[cfg(test)]
use crate::test_support::wait::{join_within, recv_within};

/// Scripted ACP agent: answers the handshake, then behaves per mode.
const AGENT: &str = r#"mode=$1; log=$2; echo $$ > "$log.pid"; held=; sid=sess-1
[ "$mode" = uuid ] && sid=7c9e6679-7425-40de-944b-e07fc1f90ae7
send() { printf '%s\n' "$1"; }
chunk() { send "{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{\"sessionId\":\"$sid\",\"update\":{\"sessionUpdate\":\"agent_message_chunk\",\"content\":{\"type\":\"text\",\"text\":\"$1\"}}}}"; }
stop() { send "{\"jsonrpc\":\"2.0\",\"id\":$1,\"result\":{\"stopReason\":\"$2\"}}"; }
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$log"
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
  *'"method":"initialize"'*) [ "$mode" = auth ] && { send "{\"jsonrpc\":\"2.0\",\"id\":$id,\"error\":{\"code\":-32000,\"message\":\"see https://login\"}}"; continue; }
    v=1; [ "$mode" = v2 ] && v=2; cap='{}'
    [ "$mode" = append ] && cap='{"_meta":{"antigravity":{"systemPrompt":{"append":true}}}}'
    send "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"protocolVersion\":$v,\"agentCapabilities\":$cap}}" ;;
  *'"method":"session/new"'*) send "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"sessionId\":\"$sid\"}}" ;;
  *'"method":"session/prompt"'*) case "$mode" in
    eof) exit 0 ;;
    hang|cancel) held=$id ;;
    deaf) exec sleep 1000 ;;
    group|gcancel) sleep 1000 & echo $! > "$log.grandchild"; held=$id ;;
    perm) held=$id; send '{"jsonrpc":"2.0","id":"p","method":"session/request_permission","params":{"options":[{"optionId":"always","kind":"allow_always"},{"optionId":"once","kind":"allow_once"}]}}' ;;
    empty) stop $id end_turn ;;
    long) i=0; while [ $i -lt 30 ]; do chunk "line $i of a long answer\\n"; i=$((i+1)); done; stop $id end_turn ;;
    *) chunk hello; stop $id end_turn ;;
    esac ;;
  *'"outcome"'*) chunk hello; stop $held end_turn ;;
  *'"method":"session/cancel"'*) case "$mode" in cancel|gcancel) stop $held cancelled ;; esac ;;
  esac
done
"#;

#[cfg(test)]
const QUICK: Deadlines = Deadlines {
    inactivity: Duration::from_secs(2),
    cancel: Duration::from_millis(300),
};

pub fn fake_launch(dir: &Path, mode: &str) -> Launch {
    let script = dir.join("agent.sh");
    std::fs::write(&script, AGENT).unwrap();
    let log = dir.join(format!("{mode}.log"));
    Launch {
        program: "/bin/sh".into(),
        resolution: None,
        args: vec![
            script.display().to_string(),
            mode.into(),
            log.display().to_string(),
        ],
        cwd: dir.into(),
        env: Vec::new(),
        unset_env: Vec::new(),
    }
}

pub fn log(dir: &Path, mode: &str) -> String {
    std::fs::read_to_string(dir.join(format!("{mode}.log"))).unwrap_or_default()
}

pub fn pid_file(dir: &Path, name: &str) -> u32 {
    let path = dir.join(name);
    let written = || std::fs::read_to_string(&path).is_ok_and(|pid| pid.ends_with('\n'));
    assert!(eventually(WAIT_CAP, written));
    std::fs::read_to_string(path)
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

pub fn eventually(cap: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + cap;
    while !done() {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    true
}

pub fn alive(pid: u32) -> bool {
    crate::services::process::unix_process_presence(pid) == ProcessGroupProbe::Live
}

/// A turn on `destination`'s production actor, run as `run_turn` runs it, cancelled by `cancel`.
pub fn acp_turn(
    destination: u64,
    dir: &Path,
    mode: &str,
    cancel: std::sync::Arc<CancelToken>,
) -> std::thread::JoinHandle<(Result<(), String>, Vec<String>)> {
    use crate::services::provider_auth_profile::ProviderAuthOverlay;
    let request = ProviderTurnRequest {
        provider: crate::services::provider::ProviderKind::Antigravity,
        prompt: "hi".into(),
        system_prompt: None,
        tool_policy: Default::default(),
        model: None,
        reasoning_effort: None,
        working_directory: dir.into(),
        session: None,
        timeout: Duration::ZERO,
        cancel: Some(cancel),
        auth_overlay: ProviderAuthOverlay::default_for(
            crate::services::provider::ProviderKind::Antigravity,
        ),
    };
    let launch = Some(fake_launch(dir, mode));
    std::thread::spawn(move || {
        let (sender, rx) = channel();
        let turn = AcpTurn {
            destination,
            request,
            shared_session: None,
            launch,
            spool: None,
        };
        let result = run_turn(turn, sender);
        (
            result,
            rx.try_iter()
                .map(|message| format!("{message:?}"))
                .collect(),
        )
    })
}

#[cfg(test)]
fn run(
    handle: &Handle,
    launch: Launch,
    shared: Option<&str>,
    system: Option<&str>,
    cancel: Option<&CancelToken>,
) -> (Result<(), String>, Vec<String>) {
    let (sender, rx) = channel();
    let (events, _) = channel();
    let request = TurnRequest {
        turn: rand::random(),
        launch,
        text: "hi".into(),
        system: system.map(str::to_owned),
        uuid_session: false,
        events,
    };
    let outputs = turn::Outputs {
        sender,
        spool: None,
        shared_session: shared.map(str::to_owned),
        cancel,
        conversation: None,
    };
    let result = turn::drive(handle, request, outputs);
    (
        result,
        rx.try_iter()
            .map(|message| format!("{message:?}"))
            .collect(),
    )
}

#[cfg(test)]
fn text(content: &str) -> String {
    format!(
        "{:?}",
        StreamMessage::Text {
            content: content.into()
        }
    )
}

fn done(result: &str, session: Option<&str>) -> String {
    let session_id = session.map(str::to_owned);
    format!(
        "{:?}",
        StreamMessage::Done {
            result: result.into(),
            session_id
        }
    )
}

#[test]
fn fake_agent_turns_map_to_the_runner_message_shapes() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let handle = actor::spawn(None, QUICK);
    let short = run(&handle, fake_launch(dir, "short"), None, None, None);
    assert_eq!(short, (Ok(()), vec![text("hello"), done("hello", None)]));

    // The shared session value goes back byte for byte; ACP's own id never replaces it.
    let shared = " conv-7 ";
    let init = StreamMessage::Init {
        session_id: shared.into(),
        raw_session_id: Some(shared.into()),
    };
    let resumed = run(&handle, fake_launch(dir, "short"), Some(shared), None, None);
    assert_eq!(
        resumed.1,
        [
            format!("{init:?}"),
            text("hello"),
            done("hello", Some(shared))
        ]
    );
    assert_eq!(
        log(dir, "short").matches("\"initialize\"").count(),
        1,
        "one resident child"
    );

    for (mode, expected) in [
        ("empty", "without any response text"),
        ("eof", "exited before the turn finished"),
        ("hang", "no activity before its deadline"),
    ] {
        let handle = actor::spawn(None, QUICK);
        let (result, messages) = run(&handle, fake_launch(dir, mode), None, None, None);
        assert_eq!(result, Ok(()), "{mode}");
        assert!(
            matches!(messages.as_slice(), [error] if error.contains(expected)),
            "{mode}: {messages:?}"
        );
    }
    let hang = pid_file(dir, "hang.log.pid");
    assert!(
        eventually(WAIT_CAP, || !alive(hang)),
        "deadline stops the child"
    );

    let v2 = run(
        &actor::spawn(None, QUICK),
        fake_launch(dir, "v2"),
        None,
        None,
        None,
    );
    assert!(
        v2.0.unwrap_err().contains("protocol version 2"),
        "a non-v1 agent is refused"
    );
    assert!(!log(dir, "v2").contains("session/new"));
    let auth = run(
        &actor::spawn(None, QUICK),
        fake_launch(dir, "auth"),
        None,
        None,
        None,
    );
    let auth = auth.0.unwrap_err();
    assert!(auth.contains("인증") && !auth.contains("https"), "{auth}");

    let perm = run(
        &actor::spawn(None, QUICK),
        fake_launch(dir, "perm"),
        None,
        None,
        None,
    );
    assert_eq!(perm.1, [text("hello"), done("hello", None)]);
    assert!(
        log(dir, "perm").contains(r#""optionId":"once""#),
        "allow_once is chosen"
    );
}

#[test]
fn system_prompt_rides_the_session_when_supported_else_the_first_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    for mode in ["append", "short"] {
        let handle = actor::spawn(None, QUICK);
        for _ in 0..2 {
            assert_eq!(
                run(&handle, fake_launch(dir, mode), None, Some("SYS"), None).0,
                Ok(())
            );
        }
        let log = log(dir, mode);
        let prompts: Vec<_> = log
            .lines()
            .filter(|l| l.contains("session/prompt"))
            .collect();
        let enveloped = prompts
            .iter()
            .filter(|line| line.contains("SYSTEM_LEN=3"))
            .count();
        let on_session = log
            .lines()
            .any(|l| l.contains("session/new") && l.contains("SYS"));
        assert_eq!(
            (prompts.len(), enveloped, on_session),
            (2, usize::from(mode == "short"), mode == "append")
        );
    }
}

#[test]
fn a_person_stop_asks_agy_to_cancel_and_teardown_or_silence_ends_the_child() {
    for (mode, source, asks, stopped) in [
        ("cancel", "explicit_stop", true, false),
        ("hang", "explicit_stop", true, true),
        ("hang", "session_teardown", false, true),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let handle = actor::spawn(
            None,
            Deadlines {
                inactivity: WAIT_CAP,
                ..QUICK
            },
        );
        let token = std::sync::Arc::new(CancelToken::new());
        let (stopper, at) = (token.clone(), dir.clone());
        let canceller = std::thread::spawn(move || {
            assert!(eventually(WAIT_CAP, || log(&at, mode).contains("session/prompt")));
            stopper.publish_cancel(source);
        });
        let turn = run(&handle, fake_launch(&dir, mode), None, None, Some(&token));
        assert_eq!(
            turn,
            (Ok(()), vec![]),
            "{source}: §4.1(a) a token-cancelled turn adds no terminal message"
        );
        join_within(canceller, WAIT_CAP);
        assert_eq!(log(&dir, mode).contains("session/cancel"), asks, "{source}");
        let pid = pid_file(&dir, &format!("{mode}.log.pid"));
        let gone = eventually(Duration::from_secs(2), || !alive(pid));
        assert_eq!(gone, stopped, "{mode} {source}");
    }
}

#[test]
fn the_agy_recording_holds_an_acp_turn_and_refuses_a_non_uuid_session() {
    use crate::services::tui_o::store::agy_spool::AgyReservation;
    for (mode, kinds) in [
        ("uuid", &["open", "init", "delta", "body_part", "done"][..]),
        ("short", &["open", "failed"]),
    ] {
        let root = tempfile::tempdir().unwrap();
        let reservation = AgyReservation {
            destination: 41,
            logical_channel: 42,
            message: 43,
            nonce: uuid::Uuid::new_v4(),
            session_key: "discord:41",
            writer_generation: std::num::NonZeroU64::MIN,
            resume: None,
        };
        let mut spool = AgySpool::reserve(root.path(), reservation).unwrap();
        let (sender, _rx) = channel();
        let (events, _) = channel();
        let launch = fake_launch(root.path(), mode);
        let request = TurnRequest {
            turn: 1,
            launch,
            text: "hi".into(),
            system: None,
            uuid_session: true,
            events,
        };
        let outputs = turn::Outputs {
            sender,
            spool: Some(&mut spool),
            shared_session: None,
            cancel: None,
            conversation: None,
        };
        let result = turn::drive(&actor::spawn(None, QUICK), request, outputs);
        assert_eq!(result.is_ok(), mode == "uuid", "{result:?}");
        assert_eq!(
            log(root.path(), mode).contains("session/prompt"),
            mode == "uuid"
        );
        let written =
            std::fs::read_to_string(root.path().join("o_store/41/agy/v1/43/0000.jsonl")).unwrap();
        let recorded: Vec<String> = written
            .lines()
            .map(|line| {
                serde_json::from_str::<serde_json::Value>(line).unwrap()["kind"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        assert_eq!(recorded, kinds, "{mode}");
    }
}

#[test]
fn a_reply_from_an_earlier_child_never_closes_the_current_turn() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path().to_path_buf();
    let handle = actor::spawn(None, QUICK);
    assert_eq!(
        run(&handle, fake_launch(&dir, "eof"), None, None, None).0,
        Ok(())
    );
    let (current, at) = (handle.clone(), dir.clone());
    let turn =
        std::thread::spawn(move || run(&current, fake_launch(&at, "hang"), None, None, None));
    assert!(eventually(WAIT_CAP, || log(&dir, "hang").contains("session/prompt")));
    // Both children numbered their prompt 3; only the second child's answer belongs here.
    let reply = |incarnation, reason: &str| Event::Line {
        incarnation,
        inbound: Inbound::Response {
            id: 3,
            outcome: Ok(serde_json::json!({"stopReason": reason})),
        },
    };
    assert!(handle.send(reply(1, "end_turn")) && handle.send(reply(2, "refusal")));
    let (result, messages) = join_within(turn, WAIT_CAP);
    assert_eq!(result, Ok(()));
    assert!(
        matches!(messages.as_slice(), [error] if error.contains("refused")),
        "{messages:?}"
    );
}

#[test]
fn a_long_answer_reaches_turn_bridge_as_the_print_path_sends_it() {
    let dir = tempfile::tempdir().unwrap();
    let conversation = "0d4f6a2e-6b8c-4f57-9a43-2f0d1b6c7e11";
    let (_, acp) = run(
        &actor::spawn(None, QUICK),
        fake_launch(dir.path(), "long"),
        Some(conversation),
        None,
        None,
    );
    let mut codec = AgyCodec::new();
    let mut lines = vec![serde_json::json!({"event": "init", "conversation_id": conversation})];
    lines.extend((0..30).map(|i| serde_json::json!({"event": "step_update", "step_type": "agent_response", "text_delta": format!("line {i} of a long answer\n")})));
    lines.push(serde_json::json!({"event": "result", "status": "SUCCESS", "conversation_id": conversation}));
    let mut print: Vec<_> = lines
        .iter()
        .flat_map(|line| codec.push_stdout_line(&line.to_string()).unwrap())
        .collect();
    print.extend(codec.finish(Some(0), "").unwrap());
    let print: Vec<_> = print.iter().map(|message| format!("{message:?}")).collect();
    assert_eq!(acp.len(), 32);
    assert_eq!(acp, print);
}

#[test]
fn hard_stop_kills_only_that_destination_child_and_its_descendants() {
    let dirs = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
    let patient = Deadlines {
        inactivity: WAIT_CAP,
        ..QUICK
    };
    let handles = [actor::spawn(None, patient), actor::spawn(None, patient)];
    let turns: Vec<_> = (0..2)
        .map(|i| {
            let (handle, launch) = (handles[i].clone(), fake_launch(dirs[i].path(), "group"));
            let (events, finished) = channel();
            let request = TurnRequest {
                turn: i as u64 + 1,
                launch,
                text: "hi".into(),
                system: None,
                uuid_session: false,
                events,
            };
            assert!(handle.send(Event::Turn(Box::new(request))));
            finished
        })
        .collect();
    let pids = |i: usize| {
        (
            pid_file(dirs[i].path(), "group.log.pid"),
            pid_file(dirs[i].path(), "group.log.grandchild"),
        )
    };
    let (stopped, kept) = (pids(0), pids(1));
    assert!(handles[0].send(Event::Terminate { turn: 1 }));
    let finished = std::iter::repeat_with(|| recv_within(&turns[0], WAIT_CAP))
        .find(|event| matches!(event, actor::TurnEvent::Finished(_)));
    let cancelled = actor::TurnEvent::Finished(Err(actor::Failure::Cancelled));
    assert_eq!(finished, Some(cancelled));
    assert!(eventually(WAIT_CAP, || !alive(stopped.0) && !alive(stopped.1)));
    assert!(alive(kept.0) && alive(kept.1) && alive(std::process::id()));
    drop(handles);
    assert!(eventually(WAIT_CAP, || !alive(kept.0) && !alive(kept.1)));
}

#[test]
fn a_forced_stop_that_agys_answer_outran_still_ends_the_child() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path().to_path_buf();
    let patient = Deadlines {
        inactivity: WAIT_CAP,
        cancel: WAIT_CAP,
    };
    let handle = actor::spawn(None, patient);
    let token = std::sync::Arc::new(CancelToken::new());
    let (stopper, at, actor) = (token.clone(), dir.clone(), handle.clone());
    let answer = std::thread::spawn(move || {
        assert!(eventually(WAIT_CAP, || log(&at, "hang").contains("session/prompt")));
        stopper.publish_cancel("explicit_stop");
        assert!(eventually(WAIT_CAP, || log(&at, "hang").contains("session/cancel")));
        // The forced stop is recorded, then agy's cancelled answer reaches the actor first.
        stopper.intend_termination(None);
        let outcome = Ok(serde_json::json!({"stopReason": "cancelled"}));
        let inbound = Inbound::Response { id: 3, outcome };
        assert!(actor.send(Event::Line {
            incarnation: 1,
            inbound
        }));
    });
    let turn = run(&handle, fake_launch(&dir, "hang"), None, None, Some(&token));
    join_within(answer, WAIT_CAP);
    assert_eq!(turn, (Ok(()), vec![]));
    let child = pid_file(&dir, "hang.log.pid");
    assert!(
        eventually(WAIT_CAP, || !alive(child)),
        "the forced stop ends the child"
    );
}

#[test]
fn an_earlier_child_exit_never_removes_the_next_child_record() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path().to_path_buf();
    let handle = actor::spawn(Some(dir.clone()), QUICK);
    let token = std::sync::Arc::new(CancelToken::new());
    let (stopper, at) = (token.clone(), dir.clone());
    let canceller = std::thread::spawn(move || {
        assert!(eventually(WAIT_CAP, || log(&at, "deaf").contains("session/prompt")));
        stopper.publish_cancel("session_teardown");
    });
    assert_eq!(
        run(&handle, fake_launch(&dir, "deaf"), None, None, Some(&token)).0,
        Ok(())
    );
    join_within(canceller, WAIT_CAP);
    // A child deaf to stdin EOF is reaped by the forced stop; the next child spawns before
    // that waiter reports.
    let first = pid_file(&dir, "deaf.log.pid");
    assert!(eventually(WAIT_CAP, || !alive(first)));
    assert_eq!(
        run(&handle, fake_launch(&dir, "short"), None, None, None).0,
        Ok(())
    );
    let next = pid_file(&dir, "short.log.pid");
    let recorded = || {
        let record = std::fs::read_to_string(dir.join(child::RECORD)).unwrap_or_default();
        record.contains(&format!("\"pid\":{next},"))
    };
    let lost = eventually(Duration::from_millis(500), || !recorded());
    assert!(!lost, "the live child keeps its record");
}

/// The leader, and a guard that ends its group when the test scope ends, also on a panic.
struct Orphan {
    child: std::process::Child,
    _group: Group,
}

impl Orphan {
    /// A process group whose leader runs `script`; the leader is reaped on its own thread.
    fn start(script: &str) -> (u32, ProcessIdentity, Self) {
        let mut command = Command::new("/bin/sh");
        crate::services::process::configure_child_process_group(&mut command);
        let child = command.args(["-c", script]).spawn().unwrap();
        let pid = child.id();
        (
            pid,
            ProcessIdentity::capture(pid),
            Self {
                child,
                _group: Group(pid),
            },
        )
    }
}

struct Group(u32);

impl Drop for Group {
    /// dash's `sh -c` keeps `sleep` as a child holding the test's output. A live group's number
    /// is never reissued, so it is still this one.
    fn drop(&mut self) {
        if process_group_probe(self.0) == ProcessGroupProbe::Live {
            crate::services::process::kill_pid_tree(self.0);
        }
    }
}

#[cfg(test)]
fn write_record(dir: &Path, pid: u32, starttime: Option<u128>, lstart: Option<u128>) -> PathBuf {
    let path = dir.join(child::RECORD);
    let record = serde_json::json!({"pid": pid, "pgid": pid, "starttime": starttime, "macos_lstart_hash": lstart});
    std::fs::write(&path, record.to_string()).unwrap();
    path
}

#[test]
fn boot_sweep_clears_only_the_recorded_group_and_holds_on_probe_errors() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let group_gone = |pid| {
        eventually(WAIT_CAP, || {
            process_group_probe(pid) == ProcessGroupProbe::Gone
        })
    };

    // Same leader: the live recorded group is stopped.
    let (pid, identity, mut leader) = Orphan::start("sleep 1000 & wait");
    let record = write_record(
        dir,
        pid,
        identity.persisted_starttime(),
        identity.persisted_macos_lstart_hash(),
    );
    let reaper = std::thread::spawn(move || leader.child.wait());
    assert_eq!(child::sweep_recorded(&record), Ok(()));
    assert!(group_gone(pid) && !record.exists());
    join_within(reaper, WAIT_CAP).unwrap();

    // Same PID but another start time: someone else's process; untouched, record dropped.
    let (pid, identity, mut other) = Orphan::start("sleep 1000");
    let wrong = identity.persisted_starttime().map(|t| t + 1);
    let lstart = identity.persisted_macos_lstart_hash().map(|t| t + 1);
    let record = write_record(dir, pid, wrong, lstart);
    assert_eq!(child::sweep_recorded(&record), Ok(()));
    assert!(alive(pid) && !record.exists());

    // An unverifiable live PID holds: record kept, the actor spawns nothing.
    let record = write_record(dir, pid, None, None);
    assert!(child::sweep_recorded(&record).is_err() && record.exists() && alive(pid));
    let launch = fake_launch(dir, "short");
    let held = run(
        &actor::spawn(Some(dir.into()), QUICK),
        launch,
        None,
        None,
        None,
    );
    assert!(held.0.unwrap_err().contains("이전 agy 정리 실패"));
    assert!(
        log(dir, "short").is_empty(),
        "no spawn while the sweep holds"
    );
    other.child.kill().unwrap();
    other.child.wait().unwrap();

    // Leader gone, a grandchild left in its group: the group is stopped, then the child spawns.
    let (pid, identity, mut leader) = Orphan::start("sleep 1000 & exit 0");
    leader.child.wait().unwrap();
    write_record(
        dir,
        pid,
        identity.persisted_starttime(),
        identity.persisted_macos_lstart_hash(),
    );
    assert_eq!(process_group_probe(pid), ProcessGroupProbe::Live);
    let (result, messages) = run(
        &actor::spawn(Some(dir.into()), QUICK),
        fake_launch(dir, "short"),
        None,
        None,
        None,
    );
    assert_eq!((result, messages.len()), (Ok(()), 2));
    assert!(group_gone(pid));
}
