//! Each cut child runs a real stop or admission until its runtime boundary reports what it returned,
//! then ends with nothing more made durable; a separate process restores what the cut left.
use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::discord::inflight::{self, InflightTurnIdentity, InflightTurnState};
use crate::services::session_host::test_support::{InjectedLivenessGuard, InjectedPresenceGuard};
use crate::services::session_host::{HostLiveness, HostPresence, HostSessionRef};
use serde_json::Value;

/// A boundary a stop reached in a cut child.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in super::super) enum Cut {
    /// The mailbox recorded the stop's intent.
    IntentRecorded,
    /// The Escape write returned and its claim settled.
    EscapeReturned,
}

const PHASE: &str = "ADK_CUT_PHASE";
const FILTER: &str = "restart_real_cutpoints_reach_their_runtime_boundaries_pg";
/// Each cut and its case number, in the order the children run.
const CUTS: [(&str, u64); 5] = [
    ("intent", 761),
    ("sent", 762),
    ("indeterminate", 763),
    ("aborted", 764),
    ("completed", 765),
];
/// The transcript's own text, which no restart terminal may show.
const TRANSCRIPT: &str = "cut-transcript-answer";
/// The completion body the row stored before its kind was admitted.
const STORED: &str = "cut-stored-answer";

static REACHED: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Ends a cut child at its own boundary with what that boundary saw.
type Exit = Box<dyn FnOnce() + Send>;
static EXIT: Mutex<Option<(Cut, Exit)>> = Mutex::new(None);

/// The delivery word the stop's own log uses for `delivery`.
fn word(delivery: Result<HerdrDelivery, HerdrNotSent>) -> &'static str {
    let delivery = delivery.unwrap_or_else(HerdrDelivery::NotSent);
    HerdrStop::Requested(delivery).observation().delivery
}

/// Records, in a cut child only, that the running stop reached `cut` and what it returned there.
pub(in super::super) fn reached(cut: Cut, delivery: Option<Result<HerdrDelivery, HerdrNotSent>>) {
    if std::env::var_os(PHASE).is_none() {
        return;
    }
    let seen = match (cut, delivery) {
        (Cut::IntentRecorded, _) => "intent_recorded".to_owned(),
        (Cut::EscapeReturned, delivery) => {
            let returned = delivery.map_or("none", word);
            format!("escape_returned:{returned}")
        }
    };
    REACHED.lock().unwrap().push(seen);
    let exit = {
        let mut slot = EXIT.lock().unwrap();
        match slot.take() {
            Some((at, exit)) if at == cut => Some(exit),
            other => {
                *slot = other;
                None
            }
        }
    };
    // The child ends at the boundary itself, so nothing the stop does next is made durable.
    let carry_on = crate::services::provider::cancel_token_claude_interrupt::herdr_interrupt_mutant;
    if let Some(exit) = exit
        && !carry_on("cut_continues_after_boundary")
    {
        exit();
    }
}

fn channel(n: u64) -> ChannelId {
    ChannelId::new(5_340_103_000 + n)
}

/// Runs this binary's cut test as `phase` in a new process; returns the pid on its `marker` line.
fn run_phase(phase: &str, root: &Path, db: &str, marker: &str) -> u32 {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([FILTER, "--test-threads=1", "--nocapture"])
        .env(PHASE, phase)
        .env("ADK_CUT_ROOT", root)
        .env("ADK_CUT_DB", db)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{phase}: {stdout}\n{stderr}");
    let line = stdout.lines().find(|line| line.contains(marker));
    let line = line.unwrap_or_else(|| panic!("{phase} printed no {marker}: {stdout}"));
    let pid = line.split("pid=").nth(1).unwrap().trim().parse().unwrap();
    assert_ne!(pid, std::process::id());
    pid
}

fn witness(root: &Path, cut: &str) -> Value {
    let path = root.join(format!("cut-{cut}.json"));
    let raw = std::fs::read(&path).unwrap_or_else(|error| panic!("{cut} left no witness: {error}"));
    serde_json::from_slice(&raw).unwrap()
}

/// Ends a cut child right after its boundary: nothing it would write next is written.
fn end_cut(root: &Path, cut: &str, witness: &Value) -> ! {
    std::fs::write(root.join(format!("cut-{cut}.json")), witness.to_string()).unwrap();
    println!("CUT_DONE {cut} pid={}", std::process::id());
    std::process::exit(0)
}

/// The cut's turn row, as its bridge would have saved it.
fn turn_row(case: &Case) -> InflightTurnState {
    let channel = case.channel.get();
    let mut row = InflightTurnState::new(
        case.provider.clone(),
        channel,
        None,
        1,
        channel + 1,
        channel + 2,
        "cut".into(),
        None,
        Some(case.owner.logical_key.clone()),
        None,
        None,
        0,
    );
    row.turn_nonce = case.token.turn_nonce().map(str::to_owned);
    row.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
    row.output_path = Some(case.path.display().to_string());
    row
}

fn row_path(case: &Case) -> PathBuf {
    let root = inflight::inflight_runtime_root().unwrap();
    inflight::inflight_state_path(&root, &case.provider, case.channel.get())
}

/// The input the executor's own observation records, with `run` as the pane write's outcome.
fn observed_input(
    case: &Case,
    run: crate::services::claude_tui::host_input::InputRun,
    submitted_at: chrono::DateTime<chrono::Utc>,
) {
    use crate::services::provider::herdr_before_start::{InputRun, claude_observed_input};
    let file = crate::services::tui_o::shadow::capture::file_identity(
        &std::fs::metadata(&case.path).unwrap(),
    );
    let start = || HerdrTurnStart {
        execution_nonce: case.record.execution_nonce.clone(),
        source: case.path.clone(),
        file: Some(file),
        offset: 0,
        submitted_at: Some(submitted_at),
    };
    let ran = claude_observed_input(Some(case.token.as_ref()), start, || run).unwrap();
    assert!(matches!(ran, InputRun::Ran(_)), "the input ran");
}

/// A marker only a child that ran past its boundary writes.
fn ran_past(root: &Path, cut: &str) -> PathBuf {
    root.join(format!("{cut}.after-boundary"))
}

/// What a held cut leaves for restart: its transcript then ends in a result no terminal admitted,
/// which restart must never show; the row and source bytes as the cut left them.
fn seal_held(case: &Case, root: &Path, cut: &str) {
    let result = serde_json::json!({"type": "result", "subtype": "success", "result": TRANSCRIPT});
    let mut source = std::fs::OpenOptions::new()
        .append(true)
        .open(&case.path)
        .unwrap();
    std::io::Write::write_all(&mut source, format!("{result}\n").as_bytes()).unwrap();
    let before = std::fs::read(row_path(case)).unwrap();
    std::fs::write(root.join(format!("{cut}.before")), before).unwrap();
    let source = std::fs::read(&case.path).unwrap();
    std::fs::write(root.join(format!("{cut}.source-before")), source).unwrap();
}

type Stop = crate::services::provider::cancel_token_claude_interrupt::HerdrInterruptState;

/// The stop's witness as its boundary sees it, then the cut's end.
fn finish_stop(case: &Case, state: &Stop, mut seen: Value, root: &Path) -> ! {
    let cut = seen["cut"].as_str().unwrap().to_owned();
    seen["escapes"] = case.escapes().into();
    seen["user_stop"] = state.user_stop.load(Ordering::Acquire).into();
    seen["turn_start"] = state.turn_start.get().is_some().into();
    seen["reached"] = serde_json::json!(*REACHED.lock().unwrap());
    seal_held(case, root, &cut);
    end_cut(root, &cut, &seen)
}

/// Runs cut `cut` of case `n` to its boundary and ends the process there.
async fn persist_cut(cut: &str, n: u64, root: &Path, pool: &sqlx::PgPool) -> ! {
    use crate::services::claude_tui::host_input::{InputRun, StopCause};
    let _switch = Switch::on();
    // Leaked: the boundary's exit may run on the stop's writer thread and never returns.
    let case: &'static Case = Box::leak(Box::new(
        Case::new(pool, root, ProviderKind::Claude, n).await,
    ));
    let _registry = case.rig.registry_on_this_thread();
    let state = case.token.herdr_interrupt_state().unwrap();
    let mut row = turn_row(case);
    let mut seen = serde_json::json!({
        "cut": cut,
        "pid": std::process::id(),
        "turn_nonce": row.turn_nonce,
        "execution_nonce": case.record.execution_nonce,
    });
    let now = chrono::Utc::now();
    match cut {
        // A cold turn: no input observation has begun.
        "intent" => state.submission.lock().unwrap().submission = HerdrSubmission::Unsubmitted,
        "sent" => observed_input(case, InputRun::Applied, now),
        "indeterminate" => {
            let cause = StopCause::Send("no reply".into());
            observed_input(
                case,
                InputRun::Indeterminate {
                    confirmed: 1,
                    cause,
                },
                now,
            );
            case.rig.leave_sends_unanswered(true);
        }
        _ => admit_cut(cut, case, &mut row, &mut seen, root).await,
    }
    inflight::save_inflight_state(&row).unwrap();
    // The input observation as the stop finds it; the stop's writer holds its lock later.
    let input = state.submission.lock().unwrap().submission;
    seen["submission"] = format!("{input:?}").into();
    let at = match cut {
        "intent" => Cut::IntentRecorded,
        _ => Cut::EscapeReturned,
    };
    let (sealed, held, dir) = (seen.clone(), state.clone(), root.to_path_buf());
    let exit: Exit = Box::new(move || finish_stop(case, &held, sealed, &dir));
    *EXIT.lock().unwrap() = Some((at, exit));
    let stop = herdr_command_stop(
        &case.shared,
        &case.provider,
        case.channel,
        &case.token,
        "/stop",
    );
    let stop = stop.await.observation();
    std::fs::write(
        ran_past(root, cut),
        format!("{}:{}", stop.intent, stop.delivery),
    )
    .unwrap();
    finish_stop(case, &state, seen, root)
}

/// The own turn's transcript and the reader's typed terminal, admitted by the bridge's admission;
/// the process ends right after that admission made the kind durable.
async fn admit_cut(
    cut: &str,
    case: &Case,
    row: &mut InflightTurnState,
    seen: &mut Value,
    root: &Path,
) -> ! {
    let input = chrono::Utc::now() - chrono::Duration::seconds(2);
    let stamped = |mut record: Value| {
        record["timestamp"] = chrono::Utc::now().to_rfc3339().into();
        format!("{record}\n")
    };
    let prompt = serde_json::json!({"type": "user", "message": {"role": "user", "content": "q"}});
    let text = |text: &str| {
        let content = serde_json::json!([{"type": "text", "text": text}]);
        serde_json::json!({"type": "assistant", "message": {"content": content}})
    };
    let aborted = cut == "aborted";
    let body = if aborted {
        let interrupt = serde_json::json!({"type": "user", "message": {"role": "user",
            "content": [{"type": "text", "text": "[Request interrupted by user]"}]}});
        stamped(prompt) + &stamped(text(TRANSCRIPT)) + &stamped(interrupt) + &stamped(text("next"))
    } else {
        let result =
            serde_json::json!({"type": "result", "subtype": "success", "result": TRANSCRIPT});
        stamped(prompt) + &stamped(text(TRANSCRIPT)) + &stamped(result)
    };
    std::fs::write(&case.path, body).unwrap();
    observed_input(
        case,
        crate::services::claude_tui::host_input::InputRun::Applied,
        input,
    );
    // The bridge's streamed body; an abort shows its notice instead, a completion this text.
    row.full_response = if aborted { TRANSCRIPT } else { STORED }.into();
    inflight::save_inflight_state(row).unwrap();
    let (mut baseline, expected) = (row.clone(), InflightTurnIdentity::from_state(row));
    let path = case.path.display().to_string();
    let read = crate::services::claude::herdr_turn::herdr_terminal_frame;
    let frame = read(&path, 0, &case.owner.logical_key, &case.token).expect("a typed terminal");
    let (durable, sealed, dir) = (row_path(case), seen.clone(), root.to_path_buf());
    let cut_name = cut.to_owned();
    use crate::services::discord::recovery_engine::herdr_admitted_restart::test_hooks;
    test_hooks::stop_after_admitted_persist(
        case.channel.get(),
        Box::new(move |kind| {
            let mut sealed = sealed;
            let row: Value = serde_json::from_slice(&std::fs::read(&durable).unwrap()).unwrap();
            sealed["reached"] = serde_json::json!([format!("admitted_persisted:{kind:?}")]);
            sealed["durable_kind"] = row["tui_terminal_kind"].clone();
            end_cut(&dir, &cut_name, &sealed)
        }),
    );
    let actor = (case.shared.as_ref(), &case.token);
    let admitted = row
        .admit_tui_terminal_frame(&mut baseline, &expected, true, actor, "", frame)
        .await;
    // Only an admission that persisted nothing returns here; its witness names no boundary.
    std::fs::write(ran_past(root, cut), "admission returned").unwrap();
    seen["admission"] = admitted.is_ok().into();
    seen["reached"] = serde_json::json!([]);
    end_cut(root, cut, seen)
}

/// Restores every cut row in a fresh process, with a fresh Herdr endpoint and Discord.
async fn restore_cuts(root: &Path, pool: &sqlx::PgPool) {
    use crate::services::discord::recovery_engine::herdr_admitted_restart::ADMITTED_ABORT_NOTICE;
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let shared = crate::services::discord::host_teardown_gate::test_support::shared_on(pool).await;
    let mut guards = Vec::new();
    for (_, n) in CUTS {
        let name = format!("AgentDesk-claude-p10-stop-{n}");
        let session = HostSessionRef::tmux(&name);
        guards.push((
            InjectedLivenessGuard::set(session, HostLiveness::DeadOrAbsent),
            InjectedPresenceGuard::set(session, HostPresence::Present),
        ));
        assert!(shared.mailbox_peek(channel(n)).is_none(), "a cold process");
    }
    let rig = HerdrRig::start();
    let _registry = rig.registry_on_this_thread();
    let discord = crate::services::discord::recovery_engine::o_cut_recorder::start(5_340_103_000);
    let discord = discord.await;
    let restore = crate::services::discord::recovery_engine::restore_inflight_turns;
    restore(&discord.http, &shared, &ProviderKind::Claude).await;
    println!("CUT_RESTORE_RETURNED pid={}", std::process::id());
    assert!(rig.sends().is_empty(), "restart replays no Escape or input");
    for (cut, n) in CUTS {
        let route = format!("/channels/{}/messages", channel(n).get());
        let calls = discord.calls().into_iter();
        let shown: Vec<String> = calls
            .filter(|call| call.route.contains(&route) && !call.route.starts_with("GET "))
            .map(|call| call.content.unwrap_or_default())
            .collect();
        let row = inflight::load_inflight_state(&ProviderKind::Claude, channel(n).get());
        let case = format!("cut={cut} shown={shown:?}");
        assert!(
            shown.iter().all(|text| !text.contains(TRANSCRIPT)),
            "{case}"
        );
        match cut {
            "aborted" | "completed" => {
                let text = if cut == "aborted" {
                    ADMITTED_ABORT_NOTICE
                } else {
                    STORED
                };
                assert!(shown.iter().any(|shown| shown.contains(text)), "{case}");
                assert!(row.is_none(), "{case}: the settled row is cleared");
            }
            _ => {
                assert!(shown.is_empty(), "{case}: a held turn writes nothing");
                let rows = inflight::inflight_runtime_root().unwrap();
                let path =
                    inflight::inflight_state_path(&rows, &ProviderKind::Claude, channel(n).get());
                let before = std::fs::read(root.join(format!("{cut}.before"))).unwrap();
                assert_eq!(
                    std::fs::read(path).ok(),
                    Some(before),
                    "{case}: the row stays"
                );
                let source = std::fs::read(root.join(format!("{cut}.source-before"))).unwrap();
                let path = root.join(format!("AgentDesk-claude-p10-stop-{n}.jsonl"));
                assert_eq!(std::fs::read(path).unwrap(), source, "{case}");
            }
        }
    }
    drop(guards);
}

/// Intent, Sent, Indeterminate and each admitted kind's durable write are reached in their own
/// processes; a fresh process keeps the unadmitted turns held and settles the admitted ones.
#[tokio::test(flavor = "current_thread")]
async fn restart_real_cutpoints_reach_their_runtime_boundaries_pg() {
    let Ok(phase) = std::env::var(PHASE) else {
        let root = tempfile::tempdir().unwrap();
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        db.connect_and_migrate().await.close().await;
        let mut pids = vec![std::process::id()];
        for (cut, _) in CUTS {
            let marker = format!("CUT_DONE {cut}");
            pids.push(run_phase(cut, root.path(), &db.database_url, &marker));
        }
        let seen: Vec<Value> = CUTS
            .iter()
            .map(|(cut, _)| witness(root.path(), cut))
            .collect();
        for (witness, (cut, _)) in seen.iter().zip(CUTS) {
            assert_eq!(witness["cut"], cut);
            assert_ne!(witness["turn_nonce"], witness["execution_nonce"], "{cut}");
        }
        let expect = |cut: &str, reached: Value| {
            let witness = &seen[CUTS.iter().position(|(name, _)| *name == cut).unwrap()];
            assert_eq!(witness["reached"], reached, "{cut}: {witness}");
            let past = std::fs::read_to_string(ran_past(root.path(), cut));
            assert!(past.is_err(), "{cut} ran past its boundary: {past:?}");
            witness.clone()
        };
        let intent = expect("intent", serde_json::json!(["intent_recorded"]));
        assert_eq!(intent["escapes"], 0, "{intent}");
        assert_eq!(intent["submission"], "Unsubmitted", "{intent}");
        assert_eq!(intent["turn_start"], false, "{intent}");
        assert_eq!(intent["user_stop"], true, "{intent}");
        for (cut, delivery, input) in [
            ("sent", "sent", "Submitted"),
            ("indeterminate", "indeterminate", "Unknown"),
        ] {
            let returned = format!("escape_returned:{delivery}");
            let witness = expect(cut, serde_json::json!(["intent_recorded", returned]));
            assert_eq!(witness["escapes"], 1, "{cut}: the socket took one Escape");
            assert_eq!(witness["submission"], input, "{witness}");
            assert_eq!(witness["turn_start"], true, "{witness}");
        }
        for (cut, kind) in [("aborted", "Aborted"), ("completed", "Completed")] {
            let reached = serde_json::json!([format!("admitted_persisted:Some({kind})")]);
            let witness = expect(cut, reached);
            assert_eq!(witness["durable_kind"], kind, "{witness}");
        }
        pids.push(run_phase(
            "RESTORE",
            root.path(),
            &db.database_url,
            "CUT_RESTORE_RETURNED",
        ));
        let count = pids.len();
        pids.sort_unstable();
        pids.dedup();
        assert_eq!(pids.len(), count, "every phase is its own process");
        db.drop().await;
        return;
    };
    let root = PathBuf::from(std::env::var_os("ADK_CUT_ROOT").unwrap());
    let _root = crate::config::set_agentdesk_root_for_test(&root);
    let _log = TestBindingRoot::enter(Some(&root));
    let url = std::env::var("ADK_CUT_DB").unwrap();
    let pool = crate::db::postgres::connect_test_pool_with_max_connections(&url, "cut", 4);
    let pool = pool.await.unwrap();
    if phase == "RESTORE" {
        restore_cuts(&root, &pool).await;
        pool.close().await;
        return;
    }
    let (_, n) = CUTS.into_iter().find(|(cut, _)| *cut == phase).unwrap();
    persist_cut(&phase, n, &root, &pool).await
}
