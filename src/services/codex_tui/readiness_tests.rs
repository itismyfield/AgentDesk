//! The Codex readiness judgment against the captured 0.169.0 timeline, and the signals that must
//! never read as ready.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde_json::{Value, json};

use super::*;
use crate::services::tui_prompt_dedupe::binding_context::{PreparedIncarnation, tests as support};

/// One Codex execution: its canonical launch context and a folder for its rollouts.
struct World {
    ctx: BindingContext,
    dir: tempfile::TempDir,
    _tmux: crate::config::TestEnvVarGuard,
    _env: [crate::config::TestEnvVarGuard; 2],
    _root: tempfile::TempDir,
}

impl World {
    /// An execution whose launch context was created at `floor`.
    fn launched(floor: DateTime<Utc>) -> Self {
        let (root, env) = support::fixture();
        let tmux = support::fake_tmux(root.path());
        let ctx = Self::context(floor);
        Self {
            ctx,
            dir: tempfile::tempdir().unwrap(),
            _tmux: tmux,
            _env: env,
            _root: root,
        }
    }

    /// Another execution's canonical context under the same runtime root.
    fn context(floor: DateTime<Utc>) -> BindingContext {
        let mut ctx = support::prepared().context;
        ctx.provider = "codex".into();
        ctx.execution_nonce = uuid::Uuid::new_v4().simple().to_string();
        ctx.created_at = floor;
        PreparedIncarnation::create(ctx).unwrap().context
    }

    fn rollout(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn hook(&self, kind: HookEventKind, turn: &str, path: &Path, at: DateTime<Utc>) {
        let payload = json!({"turn_id": turn, "transcript_path": path});
        note_hook(true, "codex", &kind, &payload, Some(&self.ctx), Some(at));
    }

    fn judge(&self, path: &Path) -> Verdict {
        self.judge_with(path, PaneLiveness::Live, Descendants::NoneObserved)
    }

    fn judge_with(&self, path: &Path, liveness: PaneLiveness, children: Descendants) -> Verdict {
        decide(
            Pane {
                liveness,
                nonce: Some(self.ctx.execution_nonce.clone()),
                floor: Some(self.ctx.created_at),
                source: path.to_path_buf(),
                children: &|| children,
            },
            Utc::now(),
        )
    }
}

fn append(path: &Path, record: &Value) {
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    writeln!(file, "{record}").unwrap();
}

fn stamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn event(at: DateTime<Utc>, kind: &str, turn: Option<&str>) -> Value {
    json!({"timestamp": stamp(at), "type": "event_msg",
        "payload": {"type": kind, "turn_id": turn, "root_turn_id": turn}})
}

/// A record a running turn writes.
fn item(at: DateTime<Utc>) -> Value {
    json!({"timestamp": stamp(at), "type": "response_item", "payload": {"type": "message",
        "role": "assistant", "content": [{"type": "output_text", "text": "x"}]}})
}

/// A launch an hour ago, and the seconds after it.
fn clock() -> (DateTime<Utc>, impl Fn(i64) -> DateTime<Utc>) {
    let floor = Utc::now() - Duration::hours(1);
    (floor, move |seconds| floor + Duration::seconds(seconds))
}

fn time(value: &Value) -> DateTime<Utc> {
    value.as_str().unwrap().parse().unwrap()
}

/// Replays each captured run in the order its records and hooks arrived, judging after each.
#[test]
fn the_captured_0_169_timeline_is_judged_step_by_step() {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/hook_payload/codex-0.169.0.json"
    ))
    .unwrap();
    let open = "busy_open_turn";
    let expected = [
        (
            "tui_bypass_startup_clear_abort",
            // Turn 1: start record, SessionStart, UserPromptSubmit, Stop, end record.
            // After /clear, turn 2 the same; turn 3 is interrupted and fires no Stop.
            vec![
                open,
                open,
                open,
                "idle_stop_hook",
                "idle",
                open,
                open,
                open,
                "idle_stop_hook",
                "idle",
                open,
                open,
                "idle",
            ],
        ),
        // A turn the model failed: its end record alone, no Stop.
        ("tui_bypass_model_error", vec![open, open, open, "idle"]),
    ];
    for (name, labels) in expected {
        let runs = fixture["runs"].as_array().unwrap();
        let run = runs.iter().find(|run| run["name"] == name).unwrap();
        // The capture is moved to an hour ago so its hooks lie inside a live execution.
        let shift = Utc::now() - Duration::hours(1) - time(&run["launched_at"]);
        let world = World::launched(time(&run["launched_at"]) + shift);
        let events = run["events"].as_array().unwrap();
        let rollout = |payload: &Value| {
            let named = Path::new(payload["transcript_path"].as_str().unwrap());
            world.rollout(named.file_name().unwrap().to_str().unwrap())
        };
        let mut steps: Vec<(DateTime<Utc>, PathBuf, Option<&Value>, &Value)> = Vec::new();
        for hook in events {
            let at = time(&hook["received_at"]) + shift;
            steps.push((at, rollout(&hook["payload"]), Some(hook), &hook["payload"]));
        }
        for record in run["rollout_turn_records"].as_array().unwrap() {
            let turn = &record["payload"]["turn_id"];
            let hook = events
                .iter()
                .find(|hook| hook["payload"]["turn_id"] == *turn);
            let path = rollout(&hook.expect("a hook names the record's turn")["payload"]);
            steps.push((time(&record["timestamp"]) + shift, path, None, record));
        }
        steps.sort_by_key(|step| step.0);
        let mut judged = Vec::new();
        for (at, path, hook, body) in steps {
            match hook {
                Some(hook) => {
                    let kind = match hook["event"].as_str().unwrap() {
                        "Stop" => HookEventKind::Stop,
                        "UserPromptSubmit" => HookEventKind::UserPromptSubmit,
                        _ => HookEventKind::SessionStart,
                    };
                    let mut payload = body.clone();
                    payload["transcript_path"] = json!(path);
                    note_hook(true, "codex", &kind, &payload, Some(&world.ctx), Some(at));
                }
                None => {
                    let mut record = body.clone();
                    record["timestamp"] = json!(stamp(at));
                    append(&path, &record);
                }
            }
            judged.push(world.judge(&path).label());
        }
        assert_eq!(judged, labels, "{name}");
    }
}

#[test]
fn a_stop_hook_ends_only_its_own_turn_while_that_turn_stays_quiet() {
    let (floor, at) = clock();
    let world = World::launched(floor);
    let path = world.rollout("rollout-a.jsonl");
    append(&path, &event(at(1), "task_started", Some("t1")));
    append(&path, &item(at(2)));
    let busy = Verdict::Busy(Busy::OpenTurn(Some("t1".into())));
    assert_eq!(world.judge(&path), busy);

    // Another turn's Stop, this turn's Stop for another rollout, and another execution's Stop.
    world.hook(HookEventKind::Stop, "t0", &path, at(3));
    world.hook(HookEventKind::Stop, "t1", &world.rollout("other"), at(3));
    let other = World::context(floor);
    let payload = json!({"turn_id": "t1", "transcript_path": path});
    let stop = HookEventKind::Stop;
    note_hook(true, "codex", &stop, &payload, Some(&other), Some(at(3)));
    assert_eq!(world.judge(&path), busy);

    world.hook(HookEventKind::Stop, "t1", &path, at(3));
    let stopped = world.judge(&path);
    assert_eq!(stopped.label(), "idle_stop_hook");
    // A Stop hook that was answered with a block: the same turn goes on.
    append(&path, &item(at(4)));
    assert_eq!(world.judge(&path), busy);
    world.hook(HookEventKind::Stop, "t1", &path, at(5));
    assert_eq!(world.judge(&path).label(), "idle_stop_hook");
    // The end record that follows the hook is the same proof.
    append(&path, &event(at(6), "task_complete", Some("t1")));
    let ended = world.judge(&path);
    assert_eq!((ended.label(), &ended), ("idle", &stopped));
}

#[test]
fn a_hook_is_evidence_only_from_inside_its_own_execution() {
    let (floor, at) = clock();
    let world = World::launched(floor);
    let path = world.rollout("rollout-a.jsonl");
    append(&path, &event(at(1), "task_started", Some("t1")));
    let payload = json!({"turn_id": "t1", "transcript_path": path});
    let (stop, start) = (HookEventKind::Stop, HookEventKind::SessionStart);
    let mut renamed = world.ctx.clone();
    renamed.tmux_session = "another-pane".into();
    let own = Some(&world.ctx);
    let inside = Some(at(2));
    let refused: [(&str, bool, &str, &HookEventKind, Value, _, _); 10] = [
        (
            "an unaccepted binding observation",
            false,
            "codex",
            &stop,
            payload.clone(),
            own,
            inside,
        ),
        (
            "another provider",
            true,
            "claude",
            &stop,
            payload.clone(),
            own,
            inside,
        ),
        (
            "another hook kind",
            true,
            "codex",
            &start,
            payload.clone(),
            own,
            inside,
        ),
        (
            "no binding context",
            true,
            "codex",
            &stop,
            payload.clone(),
            None,
            inside,
        ),
        (
            "a context that is not the stored one",
            true,
            "codex",
            &stop,
            payload.clone(),
            Some(&renamed),
            inside,
        ),
        (
            "no publish time",
            true,
            "codex",
            &stop,
            payload.clone(),
            own,
            None,
        ),
        (
            "published before the launch",
            true,
            "codex",
            &stop,
            payload.clone(),
            own,
            Some(at(-1)),
        ),
        (
            "published in the future",
            true,
            "codex",
            &stop,
            payload.clone(),
            own,
            Some(at(7200)),
        ),
        (
            "no turn id",
            true,
            "codex",
            &stop,
            json!({"transcript_path": path}),
            own,
            inside,
        ),
        (
            "no transcript path",
            true,
            "codex",
            &stop,
            json!({"turn_id": "t1"}),
            own,
            inside,
        ),
    ];
    for (name, accepted, provider, kind, payload, context, published) in refused {
        note_hook(accepted, provider, kind, &payload, context, published);
        assert_eq!(world.judge(&path).label(), "busy_open_turn", "{name}");
    }
    note_hook(true, "codex", &stop, &payload, own, inside);
    assert_eq!(world.judge(&path).label(), "idle_stop_hook");
}

#[test]
fn what_proves_no_end_is_never_ready() {
    let (floor, at) = clock();
    let world = World::launched(floor);
    let ended = [
        event(at(1), "task_started", Some("t1")),
        event(at(2), "task_complete", Some("t1")),
    ];
    let write = |name: &str, records: &[Value]| {
        let path = world.rollout(name);
        records.iter().for_each(|record| append(&path, record));
        path
    };
    let live = |path: &Path| world.judge(path);

    let path = write("ended.jsonl", &ended);
    assert_eq!(live(&path).label(), "idle");
    let gone = world.judge_with(&path, PaneLiveness::DeadOrAbsent, Descendants::NoneObserved);
    assert_eq!(gone, Verdict::Gone);
    let unread = world.judge_with(&path, PaneLiveness::ProbeError, Descendants::NoneObserved);
    assert_eq!(unread, Verdict::Unknown(Why::ProbeError));

    let missing = world.rollout("missing.jsonl");
    assert_eq!(live(&missing), Verdict::Unknown(Why::NoRollout));
    let archived = world.rollout("archived.jsonl");
    fs::write(world.rollout("archived.jsonl.zst"), b"zst").unwrap();
    assert_eq!(live(&archived), Verdict::Unknown(Why::SourceArchived));

    let unstarted = write("unstarted.jsonl", &[item(at(1))]);
    assert_eq!(live(&unstarted), Verdict::Unknown(Why::NoTurn));
    // The turn's own end, then an end that names no turn.
    let unnamed = write("unnamed.jsonl", &ended);
    append(&unnamed, &event(at(3), "task_complete", None));
    assert_eq!(live(&unnamed), Verdict::Unknown(Why::Unreadable));
    // A start line Codex has not finished writing.
    let torn = write("torn.jsonl", &ended);
    let mut file = fs::OpenOptions::new().append(true).open(&torn).unwrap();
    file.write_all(b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_sta")
        .unwrap();
    assert_eq!(live(&torn), Verdict::Unknown(Why::Unreadable));
}

#[test]
fn a_turn_from_before_the_launch_context_is_not_this_executions() {
    let (floor, at) = clock();
    let world = World::launched(floor);
    let floorless = |path: &Path| {
        let pane = Pane {
            liveness: PaneLiveness::Live,
            nonce: None,
            floor: None,
            source: path.to_path_buf(),
            children: &|| Descendants::NoneObserved,
        };
        decide(pane, Utc::now()).label()
    };
    // The resumed rollout's last turn ended before this launch, or broke off open.
    let ended = world.rollout("ended.jsonl");
    append(&ended, &event(at(-20), "task_started", Some("t0")));
    append(&ended, &event(at(-10), "task_complete", Some("t0")));
    let broken = world.rollout("broken.jsonl");
    append(&broken, &event(at(-20), "task_started", Some("t0")));
    for (path, unfloored) in [(&ended, "idle"), (&broken, "busy_open_turn")] {
        assert_eq!(world.judge(path), Verdict::Unknown(Why::NoTurn));
        assert_eq!(floorless(path), unfloored, "the floor is what set it aside");
    }
    // The execution's own turn is judged as any other.
    append(&broken, &event(at(1), "task_started", Some("t1")));
    assert_eq!(world.judge(&broken).label(), "busy_open_turn");
    append(&broken, &event(at(2), "task_complete", Some("t1")));
    assert_eq!(world.judge(&broken).label(), "idle");
}

#[test]
fn a_submitted_prompt_is_busy_until_its_turn_shows_or_the_evidence_expires() {
    let (floor, at) = clock();
    let world = World::launched(floor);
    let path = world.rollout("rollout-a.jsonl");
    append(&path, &event(at(1), "task_started", Some("t1")));
    append(&path, &event(at(2), "task_complete", Some("t1")));
    assert_eq!(world.judge(&path).label(), "idle");
    let prompt = HookEventKind::UserPromptSubmit;
    let submitted = |turn: &str| Verdict::Busy(Busy::PromptSubmitted(turn.into()));
    let with = |children| world.judge_with(&path, PaneLiveness::Live, children);

    // No start record follows: the evidence holds while the rollout moved or a child may run.
    world.hook(prompt.clone(), "t2", &path, at(3));
    assert_eq!(world.judge(&path), submitted("t2"));
    let quiet = std::time::SystemTime::now() - QUIET - std::time::Duration::from_secs(1);
    let aged = fs::File::options().append(true).open(&path).unwrap();
    aged.set_modified(quiet).unwrap();
    assert_eq!(with(Descendants::Present), submitted("t2"));
    assert_eq!(with(Descendants::Unknown), submitted("t2"));
    assert_eq!(with(Descendants::NoneObserved).label(), "idle");

    // The start record of the named turn is in the rollout before its hook arrives.
    append(&path, &event(at(4), "task_started", Some("t3")));
    world.hook(prompt, "t3", &path, at(5));
    assert_eq!(
        world.judge(&path),
        Verdict::Busy(Busy::OpenTurn(Some("t3".into())))
    );
    append(&path, &event(at(6), "task_complete", Some("t3")));
    assert_eq!(world.judge(&path).label(), "idle");
}

/// The readiness wait's own entry, over a spy pane and a rollout bound to it.
#[test]
fn the_readiness_wait_follows_the_bound_rollout_whatever_the_pane_shows() {
    use super::super::host_input::spy::{SpyGuard, SpyState};
    use super::super::input::{PromptReadinessKind, wait_until_codex_tui_input_ready};
    let (floor, at) = clock();
    let world = World::launched(floor);
    let composer = include_str!("../../../tests/fixtures/tui_input/codex-0.160.0-idle.ansi");
    let draft = composer.replace("\x1b[2mAsk Codex to do anything\x1b[0m", "a typed draft");
    assert_ne!(draft, composer);
    let started = event(at(1), "task_started", Some("t1"));
    let ended = [started.clone(), event(at(2), "task_complete", Some("t1"))];
    let wait = |name: &str, pane: Option<&str>, records: &[Value]| {
        let path = world.rollout(&format!("{name}.jsonl"));
        records.iter().for_each(|record| append(&path, record));
        crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
            name,
            crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
                runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::CodexTui,
                output_path: path.display().to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: None,
                last_offset: 0,
                relay_last_offset: None,
            },
        );
        let _spy = SpyGuard::install(SpyState {
            captures: std::iter::repeat_n(pane.map(str::to_string), 64).collect(),
            ..SpyState::default()
        });
        wait_until_codex_tui_input_ready(name, PromptReadinessKind::PostTurnHandoff, None).is_ok()
    };
    assert!(
        wait("i8082-wait-unread-ended", None, &ended),
        "a pane that cannot be read, over an ended turn, is ready"
    );
    assert!(
        !wait("i8082-wait-composer-open", Some(composer), &[started]),
        "a pane that shows an empty composer over an open turn is not ready"
    );
    assert!(
        !wait("i8082-wait-draft-ended", Some(&draft), &ended),
        "a draft that was read still blocks an ended turn"
    );
}
