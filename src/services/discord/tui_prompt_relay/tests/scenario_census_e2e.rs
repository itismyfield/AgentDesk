//! Census and production replay over `tests/e2e/tui_relay/scenarios`.
//!
//! Each scenario file is the only source of truth for its own class and for
//! why an excluded one is excluded; nothing here carries a per-scenario list.
//! The pinned census is the independent half of the comparison, so adding,
//! deleting or reclassifying a file fails here instead of quietly moving the
//! executed count.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::mpsc;

use serde_yaml::Value;

use crate::services::agent_protocol::StreamMessage;
use crate::services::codex_tui::rollout_tail::RolloutRecordDecoder;
use crate::services::session_backend::{StreamLineState, process_stream_line};

const SCENARIO_SUBDIR: &str = "tests/e2e/tui_relay/scenarios";
const RUNNABLE_CLASS: &str = "fixture";

// Pinned on purpose. Derive these from the scan instead and the census becomes
// an identity that stays green when a scenario file is deleted or added.
const CENSUS_FIXTURE: usize = 2;
const CENSUS_LIVE: usize = 30;
const CENSUS_UNSUPPORTED: usize = 3;
const CENSUS_TOTAL: usize = CENSUS_FIXTURE + CENSUS_LIVE + CENSUS_UNSUPPORTED;

/// The one runnable scenario whose declared markers production does not relay
/// (#6033). Its replay is judged by the pin test below, not by the marker rule.
const PRODUCTION_DIVERGENCE_SCENARIO: &str = "E-25";

struct Scenario {
    id: String,
    file: String,
    coverage_class: String,
    agent_mode: String,
    skip_reason: Option<String>,
    steps: Vec<Value>,
    assertions: Vec<Value>,
}

fn scenario_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(SCENARIO_SUBDIR)
}

fn text_of(node: &Value, key: &str) -> Option<String> {
    node.get(key)
        .and_then(Value::as_str)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn seq_of(node: &Value, key: &str) -> Vec<Value> {
    node.get(key)
        .and_then(Value::as_sequence)
        .cloned()
        .unwrap_or_default()
}

fn load_scenarios() -> Vec<Scenario> {
    let dir = scenario_dir();
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|error| panic!("read {}: {error}", dir.display()))
        .map(|entry| entry.expect("scenario directory entry").path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "yaml")
        })
        .collect();
    paths.sort();
    paths
        .iter()
        .map(|path| {
            let file = path
                .file_name()
                .expect("scenario file name")
                .to_string_lossy()
                .into_owned();
            let raw = std::fs::read_to_string(path)
                .unwrap_or_else(|error| panic!("read {file}: {error}"));
            let doc: Value =
                serde_yaml::from_str(&raw).unwrap_or_else(|error| panic!("parse {file}: {error}"));
            Scenario {
                id: text_of(&doc, "id").unwrap_or_else(|| panic!("{file} must declare id")),
                coverage_class: text_of(&doc, "coverage_class")
                    .unwrap_or_else(|| panic!("{file} must declare coverage_class")),
                agent_mode: text_of(&doc, "agent_mode")
                    .unwrap_or_else(|| panic!("{file} must declare agent_mode")),
                skip_reason: text_of(&doc, "skip_reason"),
                steps: seq_of(&doc, "steps"),
                assertions: seq_of(&doc, "assertions"),
                file,
            }
        })
        .collect()
}

fn is_runnable(scenario: &Scenario) -> bool {
    scenario.coverage_class == RUNNABLE_CLASS
}

/// The reason an excluded scenario is excluded, read back out of its own file.
fn exclusion_reason(scenario: &Scenario) -> String {
    match scenario.coverage_class.as_str() {
        "live" => format!(
            "live: agent_mode={} needs a live provider session",
            scenario.agent_mode
        ),
        "unsupported-known-gap" => {
            let declared = scenario.skip_reason.as_deref().unwrap_or_else(|| {
                panic!(
                    "{} declares coverage_class=unsupported-known-gap and must carry skip_reason",
                    scenario.file
                )
            });
            let head = declared.split_whitespace().take(9).collect::<Vec<_>>();
            format!("unsupported-known-gap: {}...", head.join(" "))
        }
        other => panic!(
            "{} declares unknown coverage_class={other:?}",
            scenario.file
        ),
    }
}

fn class_tally(scenarios: &[Scenario]) -> BTreeMap<String, usize> {
    let mut tally = BTreeMap::new();
    for scenario in scenarios {
        *tally.entry(scenario.coverage_class.clone()).or_insert(0) += 1;
    }
    tally
}

fn to_json(frame: &Value) -> serde_json::Value {
    serde_json::to_value(frame).expect("scenario frame is JSON representable")
}

fn push_text(out: &mut String, message: &StreamMessage) {
    match message {
        StreamMessage::Text { content } => out.push_str(content),
        StreamMessage::Done { result, .. } => out.push_str(result),
        StreamMessage::TaskNotification { summary, .. } => out.push_str(summary),
        _ => return,
    }
    out.push('\n');
}

fn replay_claude(frames: &[Value]) -> String {
    let (sender, receiver) = mpsc::channel();
    let mut state = StreamLineState::new();
    for frame in frames {
        let line = to_json(frame).to_string();
        assert!(
            process_stream_line(&line, &sender, &mut state),
            "production Claude parser closed the channel on {line}"
        );
    }
    drop(sender);
    let mut out = String::new();
    for message in receiver.try_iter() {
        push_text(&mut out, &message);
    }
    if let Some(result) = state.final_result.as_ref() {
        out.push_str(result);
        out.push('\n');
    }
    out
}

fn replay_codex(frames: &[Value]) -> String {
    let mut decoder = RolloutRecordDecoder::default();
    let mut out = String::new();
    for frame in frames {
        if let Some(messages) = decoder.decode(&to_json(frame)) {
            for message in &messages {
                push_text(&mut out, message);
            }
        }
    }
    // `task_complete.last_agent_message` reaches `final_text` only once the
    // explicit completion policy promotes it, which is the consuming call.
    if let Ok(completed) = decoder.completed_response()
        && !completed.is_empty()
        && !out.contains(completed.as_str())
    {
        out.push_str(&completed);
        out.push('\n');
    }
    out
}

/// Replays a runnable scenario's declared frames through the production stream
/// parsers and returns everything they relayed.
fn production_replay(scenario: &Scenario) -> String {
    let mut produced = String::new();
    let mut replays = 0usize;
    for step in &scenario.steps {
        let Some(spec) = step.get("replay_fixture") else {
            continue;
        };
        let provider = text_of(spec, "provider")
            .unwrap_or_else(|| panic!("{} replay_fixture must declare provider", scenario.file));
        let frames = seq_of(spec, "frames");
        assert!(
            !frames.is_empty(),
            "{} replay_fixture must declare frames",
            scenario.file
        );
        produced.push_str(&match provider.as_str() {
            "claude" => replay_claude(&frames),
            "codex" => replay_codex(&frames),
            other => panic!("{} replay_fixture provider {other:?}", scenario.file),
        });
        replays += 1;
    }
    assert!(
        replays > 0,
        "{} is classed {RUNNABLE_CLASS} but declares no replay_fixture step",
        scenario.file
    );
    produced
}

fn declared_markers(scenario: &Scenario) -> Vec<String> {
    scenario
        .assertions
        .iter()
        .filter_map(|spec| text_of(spec, "text_present"))
        .collect()
}

/// The body #6033 drops, read out of the scenario's own `task_complete` frame.
///
/// Deliberately not the marker list: a scenario may also declare `text_present`
/// for streamed text that production has always relayed, and such a marker says
/// nothing about this defect. Pinning on the marker list would turn "someone
/// added a streaming assertion" into "#6033 is fixed, drop the pin", which
/// deletes the only regression pin while the defect is still live.
fn task_complete_final_body(scenario: &Scenario) -> Vec<String> {
    let mut lines = Vec::new();
    for step in &scenario.steps {
        let Some(spec) = step.get("replay_fixture") else {
            continue;
        };
        for frame in seq_of(spec, "frames") {
            let Some(body) = frame
                .get("payload")
                .and_then(|payload| payload.get("last_agent_message"))
                .and_then(Value::as_str)
            else {
                continue;
            };
            lines.extend(
                body.lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .map(str::to_string),
            );
        }
    }
    lines
}

#[test]
fn scenario_census_matches_the_pinned_class_distribution() {
    let scenarios = load_scenarios();
    let tally = class_tally(&scenarios);
    let expected = BTreeMap::from([
        (RUNNABLE_CLASS.to_string(), CENSUS_FIXTURE),
        ("live".to_string(), CENSUS_LIVE),
        ("unsupported-known-gap".to_string(), CENSUS_UNSUPPORTED),
    ]);
    assert_eq!(
        tally,
        expected,
        "scenario class distribution drifted; {} files under {}",
        scenarios.len(),
        SCENARIO_SUBDIR
    );
    assert_eq!(
        scenarios.len(),
        CENSUS_TOTAL,
        "scenario file count drifted from the pinned census"
    );
}

#[test]
fn every_excluded_scenario_names_its_reason_in_its_own_file() {
    let scenarios = load_scenarios();
    let mut by_reason: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for scenario in scenarios.iter().filter(|scenario| !is_runnable(scenario)) {
        by_reason
            .entry(exclusion_reason(scenario))
            .or_default()
            .push(scenario.id.clone());
    }
    let excluded: usize = by_reason.values().map(Vec::len).sum();
    println!("excluded {excluded} of {CENSUS_TOTAL} scenarios:");
    for (reason, ids) in &by_reason {
        println!("  {:>2} x {reason} [{}]", ids.len(), ids.join(", "));
    }
    assert_eq!(
        excluded,
        CENSUS_TOTAL - CENSUS_FIXTURE,
        "excluded count drifted from the pinned census"
    );
}

#[test]
fn runnable_scenarios_replay_their_declared_markers_through_production_parsers() {
    let scenarios = load_scenarios();
    let runnable: Vec<&Scenario> = scenarios.iter().filter(|s| is_runnable(s)).collect();
    assert_eq!(
        runnable.len(),
        CENSUS_FIXTURE,
        "runnable scenario count drifted from the pinned census"
    );
    // Without this the #6033 pin below could rot silently: if E-25 were deleted
    // or reclassified, that test would stay green while guarding nothing.
    assert!(
        runnable
            .iter()
            .any(|scenario| scenario.id == PRODUCTION_DIVERGENCE_SCENARIO),
        "{PRODUCTION_DIVERGENCE_SCENARIO} left the runnable set; its #6033 pin now guards nothing"
    );
    let mut executed = 0usize;
    for scenario in runnable
        .iter()
        .filter(|scenario| scenario.id != PRODUCTION_DIVERGENCE_SCENARIO)
    {
        let produced = production_replay(scenario);
        let markers = declared_markers(scenario);
        assert!(
            !markers.is_empty(),
            "{} declares no text_present assertion to judge its replay by",
            scenario.file
        );
        for marker in &markers {
            assert!(
                produced.contains(marker.as_str()),
                "{} replayed through the production parser without {marker:?}; produced={produced:?}",
                scenario.file
            );
        }
        println!(
            "ran {} ({}) markers={}",
            scenario.id,
            scenario.file,
            markers.len()
        );
        executed += 1;
    }
    assert_eq!(
        executed,
        CENSUS_FIXTURE - 1,
        "executed scenario count must equal the pinned runnable census minus the #6033 pin"
    );
}

/// This assertion pins a DEFECT, not a behaviour worth keeping: production drops
/// `task_complete.last_agent_message` when it neither supersedes nor mirrors the
/// streamed text (#6033). When production is fixed this test goes red — that is
/// correct. Invert the assertion then and delete the constant it reads.
///
/// It is pinned on that frame's body, not on the scenario's marker list, so the
/// only way it can go red is the one the message names.
#[test]
fn e25_task_complete_final_body_is_dropped_pending_6033() {
    let scenarios = load_scenarios();
    let scenario = scenarios
        .iter()
        .find(|scenario| scenario.id == PRODUCTION_DIVERGENCE_SCENARIO)
        .unwrap_or_else(|| panic!("{PRODUCTION_DIVERGENCE_SCENARIO} must still exist"));
    let produced = production_replay(scenario);
    let body = task_complete_final_body(scenario);
    assert!(
        !body.is_empty(),
        "{} declares no task_complete.last_agent_message body to pin",
        scenario.file
    );
    let relayed: Vec<&String> = body
        .iter()
        .filter(|line| produced.contains(line.as_str()))
        .collect();
    assert!(
        relayed.is_empty(),
        "#6033 is fixed: production now relays the task_complete.last_agent_message body {relayed:?} \
         for {}. Invert this assertion and delete {PRODUCTION_DIVERGENCE_SCENARIO}'s constant.",
        scenario.file
    );
    assert!(
        !produced.is_empty(),
        "{} relayed nothing at all through the production parser",
        scenario.file
    );
    println!(
        "pinned {} ({}): {} task_complete.last_agent_message lines dropped by production (#6033)",
        scenario.id,
        scenario.file,
        body.len()
    );
}
