//! The pane history the binding log folds decides left sessions, returns and stale hooks; each
//! hook carries the relay publish time the receiver reads from its header.

use super::*;
use crate::services::claude_tui::hook_server::observation_ingress::{
    IngressOutcome, observe_binding_hook,
};
use crate::services::claude_tui::hook_server::relay_receipts::RELAY_PUBLISHED_AT_HEADER;
use crate::services::claude_tui::source_verify::SourceHistory;
use crate::services::tui_prompt_dedupe::binding_events::{BindingCause, claude_history};
use crate::services::tui_prompt_dedupe::{
    BEFORE_AUTHORITY, claude_session_rotation_for_tmux, register_tmux_runtime_binding,
};
use chrono::{DateTime, Utc};

const T0: i64 = 1_800_000_000;
const ADOPTED: AdoptionHttp = AdoptionHttp::Durable(DurableKind::Adopted);
const PENDING: AdoptionHttp = AdoptionHttp::Durable(DurableKind::Pending);
const REGRESSION: AdoptionHttp =
    AdoptionHttp::Skipped(AdoptSkip::SourceRejected(SourceRejection::Regression));
const CONFLICT: AdoptionHttp = AdoptionHttp::Skipped(AdoptSkip::ResumeConflict);
/// What a waiting Pending's own later hook answers once it resolves.
const RESOLVED: AdoptionHttp = AdoptionHttp::Durable(DurableKind::AlreadyRecorded);

fn at(secs: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(T0 + secs, 0).unwrap()
}

fn signal(event: &str, source: Option<&str>, path: &Path, secs: Option<i64>) -> HookSignal {
    let payload = serde_json::json!({ "source": source, "transcript_path": path });
    HookSignal {
        published_at: secs.map(at),
        ..HookSignal::from_payload(event, &payload)
    }
}

fn start(source: &str, path: &Path, secs: i64) -> HookSignal {
    signal("session_start", Some(source), path, Some(secs))
}

fn stop(path: &Path, secs: i64) -> HookSignal {
    signal("stop", None, path, Some(secs))
}

fn prompt(path: &Path, secs: i64) -> HookSignal {
    signal("user_prompt_submit", None, path, Some(secs))
}

/// Moves `path`'s mtime `secs` away from now.
fn shift_mtime(path: &Path, secs: i64) {
    let now = Utc::now().timestamp() + secs;
    filetime::set_file_mtime(path, filetime::FileTime::from_unix_time(now, 0)).unwrap();
}

fn kind(event: &BindingEvent) -> String {
    match &event.new {
        BindingTarget::Source(s) => format!("source:{}", s.session_id),
        BindingTarget::Pending {
            payload_session_id, ..
        } => format!("pending:{payload_session_id}"),
        BindingTarget::Resolved { source, .. } => format!("resolved:{}", source.session_id),
        BindingTarget::Rejected {
            payload_session_id,
            reason,
            ..
        } => format!("rejected:{payload_session_id}:{reason}"),
    }
}

/// A pane launched on A under a fresh spawn nonce, logging to its channel.
struct Pane<'l> {
    lane: &'l Lane,
    channel: u64,
    tmux: &'static str,
    a: String,
}

impl<'l> Pane<'l> {
    fn new(lane: &'l Lane, channel: u64, tmux: &'static str) -> Self {
        let a = launched(lane, channel, tmux);
        Self {
            lane,
            channel,
            tmux,
            a,
        }
    }

    fn file(&self, session: &str) -> PathBuf {
        self.lane.touch(session)
    }

    fn path(&self, session: &str) -> PathBuf {
        self.lane.path(session)
    }

    fn send(&self, session: &str, hook: HookSignal) -> AdoptionHttp {
        adopt_from_hook(&self.a, session, &hook)
    }

    /// Adopts each session in turn with a prompt published at its time.
    fn chain(&self, steps: &[(&str, i64)]) {
        for (session, secs) in steps {
            let hook = prompt(&self.file(session), *secs);
            assert_eq!(self.send(session, hook), ADOPTED, "chain step {secs}");
            // Delivery drains each rotation before the next hook, as a settled pane does.
            assert!(clear_claude_session_rotation(self.tmux));
        }
    }

    fn bound(&self) -> Option<String> {
        bound_session(self.tmux)
    }

    fn log(&self) -> Vec<BindingEvent> {
        records_strict(self.channel).unwrap().unwrap()
    }

    fn kinds(&self) -> Vec<String> {
        self.log().iter().map(kind).collect()
    }

    fn naming(&self, session: &str) -> Vec<String> {
        let kinds = self.kinds().into_iter();
        kinds.filter(|k| k.contains(session)).collect()
    }

    fn history(&self) -> SourceHistory {
        let nonce = match observe_spawn_nonce_marker(self.tmux) {
            crate::services::tui_prompt_dedupe::binding_context::SpawnNonceMarker::Known(n) => {
                Some(n)
            }
            _ => None,
        };
        claude_history(self.channel, self.tmux, nonce.as_deref())
            .unwrap()
            .1
    }

    fn raw_lines(&self) -> Vec<serde_json::Value> {
        let text = fs::read_to_string(self.lane.log(self.channel)).unwrap();
        text.lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    /// Relaunches the pane on `session` as a new execution.
    fn relaunch(&self, session: &str) {
        stamp(self.tmux);
        let path = self.file(session);
        register_launched_tmux_runtime_binding(self.tmux, claude(&path, session));
    }
}

fn published(value: &serde_json::Value) -> Option<String> {
    value["published_at"].as_str().map(str::to_owned)
}

fn rfc(secs: i64) -> String {
    serde_json::to_value(at(secs))
        .unwrap()
        .as_str()
        .unwrap()
        .to_owned()
}

#[test]
fn a_late_hook_of_a_left_session_is_refused_even_with_the_newest_mtime() {
    let lane = Lane::new();
    let (b, c) = (uuid(), uuid());
    let pane = Pane::new(&lane, 7_700, "n2b-h1");
    pane.chain(&[(&b, 10), (&c, 20)]);
    shift_mtime(&pane.path(&b), 3600);
    for _ in 0..2 {
        assert_eq!(
            pane.send(&b, stop(&pane.path(&b), 15)),
            REGRESSION,
            "[H1:refused]"
        );
    }
    let refused = format!("rejected:{b}:regression");
    let after_c = pane
        .kinds()
        .into_iter()
        .skip_while(|k| *k != format!("source:{c}"));
    assert_eq!(
        after_c.collect::<Vec<_>>(),
        [format!("source:{c}"), refused],
        "[H1:refused]"
    );
    assert_eq!(pane.bound(), Some(c), "[H1:refused]");
}

#[test]
fn a_resume_back_to_a_left_session_is_adopted_at_its_session_start() {
    let lane = Lane::new();
    let (b, c) = (uuid(), uuid());
    let pane = Pane::new(&lane, 7_701, "n2b-h2");
    pane.chain(&[(&b, 10), (&c, 20)]);
    shift_mtime(&pane.path(&b), -3600);
    assert_eq!(
        pane.send(&b, start("resume", &pane.path(&b), 30)),
        ADOPTED,
        "[H2:adopted]"
    );
    assert_eq!(pane.bound(), Some(b.clone()), "[H2:adopted]");
    let last = pane.log().pop().unwrap();
    assert_eq!(
        (kind(&last), last.cause),
        (format!("source:{b}"), BindingCause::Resume)
    );
    assert_eq!(last.old.map(|o| o.session_id), Some(c.clone()));
    let pin = pinned_source(pane.channel, pane.tmux).unwrap();
    assert_eq!(pin.map(|p| p.session_id), Some(b), "[H2:adopted] verified");
    assert_eq!(published(pane.raw_lines().last().unwrap()), Some(rfc(30)));
    let rotation = claude_session_rotation_for_tmux(pane.tmux).unwrap();
    assert_eq!(rotation.old_session_id, Some(c), "[H2:adopted]");
}

#[test]
fn a_replayed_resume_published_before_the_switch_is_refused() {
    let lane = Lane::new();
    let (b, c) = (uuid(), uuid());
    let pane = Pane::new(&lane, 7_702, "n2b-h3");
    pane.chain(&[(&b, 10), (&c, 20)]);
    shift_mtime(&pane.path(&b), 3600);
    let replay = start("resume", &pane.path(&b), 15);
    assert_eq!(pane.send(&b, replay), REGRESSION, "[H3:refused]");
    assert_eq!(pane.bound(), Some(c), "[H3:refused]");
}

#[test]
fn a_resume_without_a_publish_time_stays_unresolved_and_alarms() {
    let lane = Lane::new();
    let (b, c) = (uuid(), uuid());
    let pane = Pane::new(&lane, 7_703, "n2b-h4");
    pane.chain(&[(&b, 10), (&c, 20)]);
    shift_mtime(&pane.path(&b), 3600);
    let resume = signal("session_start", Some("resume"), &pane.path(&b), None);
    assert_eq!(pane.send(&b, resume), CONFLICT, "[H4:conflict]");
    let last = pane.kinds().pop().unwrap();
    assert_eq!(
        last,
        format!("rejected:{b}:resume_conflict"),
        "[H4:conflict]"
    );
    assert_eq!(pane.bound(), Some(c), "[H4:conflict]");
}

#[test]
fn a_superseded_pending_is_refused_when_its_file_appears_late() {
    let lane = Lane::new();
    let (b, c) = (uuid(), uuid());
    let pane = Pane::new(&lane, 7_704, "n2b-h5");
    assert_eq!(pane.send(&b, start("clear", &pane.path(&b), 10)), PENDING);
    assert_eq!(pane.send(&c, prompt(&pane.file(&c), 20)), ADOPTED);
    pane.file(&b);
    shift_mtime(&pane.path(&b), 3600);
    assert_eq!(
        pane.send(&b, stop(&pane.path(&b), 15)),
        REGRESSION,
        "[H5:refused]"
    );
    assert_eq!(pane.bound(), Some(c), "[H5:refused]");
}

#[test]
fn left_sessions_survive_a_restart_of_the_same_execution() {
    let lane = Lane::new();
    let (b, c) = (uuid(), uuid());
    let pane = Pane::new(&lane, 7_705, "n2b-h6");
    pane.chain(&[(&b, 10), (&c, 20)]);
    forget_channel_for_tests(pane.channel);
    shift_mtime(&pane.path(&b), 3600);
    assert_eq!(
        pane.send(&b, stop(&pane.path(&b), 15)),
        REGRESSION,
        "[H6:refused]"
    );
    assert_eq!(pane.bound(), Some(c), "[H6:refused]");
}

#[test]
fn a_new_execution_does_not_inherit_left_sessions() {
    let lane = Lane::new();
    let (b, c, d) = (uuid(), uuid(), uuid());
    let pane = Pane::new(&lane, 7_706, "n2b-h7");
    pane.chain(&[(&b, 10), (&c, 20)]);
    pane.relaunch(&d);
    let history = pane.history();
    assert!(
        history.left.is_empty() && history.awaiting.is_none(),
        "[H7:reset] {history:?}"
    );
    assert_eq!(history.current.map(|c| c.session), Some(d), "[H7:reset]");
    assert_eq!(
        pane.send(&b, start("resume", &pane.path(&b), 50)),
        ADOPTED,
        "[H7:adopted]"
    );
}

#[test]
fn a_new_execution_adopts_a_hook_of_a_session_the_old_execution_left() {
    let lane = Lane::new();
    let (b, c, d) = (uuid(), uuid(), uuid());
    let pane = Pane::new(&lane, 7_707, "n2b-h7b");
    pane.chain(&[(&b, 10), (&c, 20)]);
    pane.relaunch(&d);
    assert_eq!(
        pane.send(&b, stop(&pane.path(&b), 50)),
        ADOPTED,
        "[H7b:adopted]"
    );
    assert_eq!(pane.bound(), Some(b), "[H7b:adopted]");
}

#[test]
fn a_new_session_with_an_older_mtime_is_still_adopted() {
    let lane = Lane::new();
    let (b, c) = (uuid(), uuid());
    let pane = Pane::new(&lane, 7_708, "n2b-h8");
    pane.chain(&[(&b, 10)]);
    assert_eq!(pane.send(&c, start("clear", &pane.path(&c), 20)), PENDING);
    pane.file(&c);
    shift_mtime(&pane.path(&c), -3600);
    assert_eq!(
        pane.send(&c, prompt(&pane.path(&c), 25)),
        RESOLVED,
        "[H8:adopted]"
    );
    assert_eq!(
        pane.kinds().pop(),
        Some(format!("resolved:{c}")),
        "[H8:adopted]"
    );
}

#[test]
fn a_hook_published_before_the_bound_switch_is_refused_even_unseen() {
    let lane = Lane::new();
    let (s, x) = (uuid(), uuid());
    let pane = Pane::new(&lane, 7_709, "n2b-h9");
    pane.chain(&[(&s, 20)]);
    pane.file(&x);
    shift_mtime(&pane.path(&x), 3600);
    assert_eq!(
        pane.send(&x, stop(&pane.path(&x), 10)),
        REGRESSION,
        "[H9:refused]"
    );
    assert_eq!(pane.bound(), Some(s), "[H9:refused]");
}

#[test]
fn live_history_equals_the_history_folded_after_a_restart() {
    let lane = Lane::new();
    let (b, c, d, f, g) = (uuid(), uuid(), uuid(), uuid(), uuid());
    let pane = Pane::new(&lane, 7_710, "n2b-h10");
    pane.chain(&[(&b, 10)]);
    assert_eq!(pane.send(&c, start("clear", &pane.path(&c), 20)), PENDING);
    let (tmux, channel) = (pane.tmux, pane.channel);
    assert!(register_rehydrated_tmux_runtime_binding(
        "claude",
        tmux,
        channel,
        claude(&pane.file(&c), &c)
    ));
    assert!(register_rehydrated_tmux_runtime_binding(
        "claude",
        tmux,
        channel,
        claude(&pane.file(&d), &d)
    ));
    assert_eq!(pane.send(&b, stop(&pane.path(&b), 15)), REGRESSION);
    assert_eq!(pane.send(&f, start("clear", &pane.path(&f), 30)), PENDING);
    assert_eq!(pane.send(&g, prompt(&pane.file(&g), 40)), ADOPTED);
    let live = pane.history();
    forget_channel_for_tests(channel);
    assert_eq!(pane.history(), live, "[H10:equal]");
    assert!(
        live.left.contains_key(&f) && live.left.contains_key(&b),
        "[H10:equal] {live:?}"
    );
    assert_eq!(
        pane.send(&b, stop(&pane.path(&b), 15)),
        REGRESSION,
        "[H10:equal]"
    );
}

#[test]
fn a_corrupt_line_marks_only_the_execution_it_touches_incomplete() {
    let lane = Lane::new();
    let (b, c, d, e, f) = (uuid(), uuid(), uuid(), uuid(), uuid());
    // (a) A corrupt line of an earlier execution leaves the next one complete.
    let pane = Pane::new(&lane, 7_711, "n2b-h11a");
    pane.chain(&[(&b, 10), (&c, 20)]);
    lane.edit_line(pane.channel, 2, Some("{not json"));
    pane.relaunch(&d);
    forget_channel_for_tests(pane.channel);
    assert!(pane.history().complete, "[H11:a]");
    // (b) An unreadable tail may be the next execution's first record.
    let mut log = fs::OpenOptions::new()
        .append(true)
        .open(lane.log(pane.channel))
        .unwrap();
    std::io::Write::write_all(&mut log, b"{torn but terminated\n").unwrap();
    forget_channel_for_tests(pane.channel);
    stamp(pane.tmux);
    assert!(!pane.history().complete, "[H11:b]");
    // (c) A lost first record of this execution leaves it incomplete, so no return is proven.
    let pane = Pane::new(&lane, 7_712, "n2b-h11c");
    pane.chain(&[(&e, 10), (&f, 20)]);
    lane.edit_line(pane.channel, 1, Some("{not json"));
    forget_channel_for_tests(pane.channel);
    assert!(!pane.history().complete, "[H11:c]");
    assert_eq!(
        pane.send(&e, prompt(&pane.path(&e), 50)),
        CONFLICT,
        "[H11:c]"
    );
    assert_eq!(pane.bound(), Some(f), "[H11:c]");
}

#[test]
fn a_resume_proof_survives_a_restart_between_the_switch_and_the_resume() {
    let lane = Lane::new();
    let (b, c) = (uuid(), uuid());
    let pane = Pane::new(&lane, 7_713, "n2b-h12");
    pane.chain(&[(&b, 10), (&c, 20)]);
    shift_mtime(&pane.path(&b), -3600);
    forget_channel_for_tests(pane.channel);
    assert_eq!(
        pane.send(&b, start("resume", &pane.path(&b), 30)),
        ADOPTED,
        "[H12:adopted]"
    );
    assert_eq!(pane.bound(), Some(b), "[H12:adopted]");
}

/// Sends a hook through the receiver's binding judgment with its relay publish time header.
fn ingress(
    pane: &Pane,
    event: &str,
    session: &str,
    payload: serde_json::Value,
    secs: i64,
) -> IngressOutcome {
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        RELAY_PUBLISHED_AT_HEADER,
        at(secs).to_rfc3339().parse().unwrap(),
    );
    observe_binding_hook(
        "claude",
        event,
        Some(&pane.a),
        Some(session),
        &payload,
        &headers,
    )
}

#[test]
fn a_hook_adopting_c_while_another_hook_is_judged_cannot_bring_back_a_left_session() {
    let lane = Lane::new();
    let (b, c) = (uuid(), uuid());
    let pane = Pane::new(&lane, 7_714, "n2b-h13");
    pane.chain(&[(&b, 10)]);
    let (a, c_path) = (pane.a.clone(), pane.file(&c));
    let c_session = c.clone();
    BEFORE_AUTHORITY.set(Some(Box::new(move || {
        assert_eq!(
            adopt_from_hook(&a, &c_session, &prompt(&c_path, 20)),
            ADOPTED
        );
    })));
    let payload = serde_json::json!({ "transcript_path": pane.path(&b) });
    let outcome = ingress(&pane, "Stop", &b, payload, 25);
    assert_eq!(
        pane.bound(),
        Some(c.clone()),
        "[H13:consistent] {outcome:?}"
    );
    let history = pane.history();
    assert!(
        history.left.contains_key(&b),
        "[H13:consistent] {history:?}"
    );
    assert_eq!(
        pane.kinds().pop(),
        Some(format!("rejected:{b}:regression")),
        "[H13:consistent]"
    );
}

#[test]
fn a_resume_published_after_a_late_delivered_switch_is_adopted() {
    let lane = Lane::new();
    let (b, c) = (uuid(), uuid());
    let pane = Pane::new(&lane, 7_715, "n2b-h14");
    pane.chain(&[(&b, 10)]);
    // C's prompt was published at 20 but only received at 100, after B's resume was published.
    let late = HookSignal {
        received_at: at(100),
        ..prompt(&pane.file(&c), 20)
    };
    assert_eq!(pane.send(&c, late), ADOPTED);
    assert_eq!(
        pane.send(&b, start("resume", &pane.path(&b), 25)),
        ADOPTED,
        "[H14:adopted]"
    );
}

#[test]
fn a_late_delivered_switch_keeps_its_publish_time_across_a_restart() {
    let lane = Lane::new();
    let (b, c) = (uuid(), uuid());
    let pane = Pane::new(&lane, 7_716, "n2b-h14r");
    pane.chain(&[(&b, 10)]);
    let late = HookSignal {
        received_at: at(100),
        ..prompt(&pane.file(&c), 20)
    };
    assert_eq!(pane.send(&c, late), ADOPTED);
    forget_channel_for_tests(pane.channel);
    assert_eq!(
        published(pane.raw_lines().last().unwrap()),
        Some(rfc(20)),
        "[H14r:sidecar]"
    );
    assert_eq!(
        pane.send(&b, start("resume", &pane.path(&b), 25)),
        ADOPTED,
        "[H14r:adopted]"
    );
}

#[test]
fn the_relay_publish_time_header_reaches_the_binding_judgment() {
    let lane = Lane::new();
    let (b, c) = (uuid(), uuid());
    let pane = Pane::new(&lane, 7_717, "n2b-h15");
    pane.chain(&[(&b, 10), (&c, 20)]);
    let payload = serde_json::json!({ "source": "resume", "transcript_path": pane.path(&b) });
    let outcome = ingress(&pane, "SessionStart", &b, payload, 30);
    assert_eq!(
        outcome,
        IngressOutcome::Durable(DurableKind::Adopted),
        "[H15:sidecar]"
    );
    assert_eq!(
        published(pane.raw_lines().last().unwrap()),
        Some(rfc(30)),
        "[H15:sidecar]"
    );
}

#[test]
fn an_observed_rebind_does_not_bar_the_hooked_session() {
    let lane = Lane::new();
    let c = uuid();
    let pane = Pane::new(&lane, 7_718, "n2b-h16");
    pane.chain(&[(&c, 20)]);
    let a_path = pane.path(&pane.a);
    let a = pane.a.clone();
    assert!(register_rehydrated_tmux_runtime_binding(
        "claude",
        pane.tmux,
        pane.channel,
        claude(&a_path, &a)
    ));
    assert_eq!(
        pane.send(&c, stop(&pane.path(&c), 30)),
        ADOPTED,
        "[H16:adopted]"
    );
}

#[test]
fn a_prompt_proves_a_return_when_the_session_start_was_dropped() {
    let lane = Lane::new();
    let (b, c) = (uuid(), uuid());
    let pane = Pane::new(&lane, 7_719, "n2b-h19");
    pane.chain(&[(&b, 10), (&c, 20)]);
    shift_mtime(&pane.path(&b), 3600);
    assert_eq!(
        pane.send(&b, prompt(&pane.path(&b), 15)),
        REGRESSION,
        "[H19:stale]"
    );
    assert_eq!(
        pane.send(&b, stop(&pane.path(&b), 30)),
        REGRESSION,
        "[H19:stop_refused]"
    );
    assert_eq!(pane.bound(), Some(c), "[H19:stop_refused]");
    assert_eq!(
        pane.send(&b, prompt(&pane.path(&b), 31)),
        ADOPTED,
        "[H19:adopted]"
    );
    let last = pane.log().pop().unwrap();
    assert_eq!(
        (kind(&last), last.cause),
        (format!("source:{b}"), BindingCause::Unknown)
    );
    assert_eq!(pane.bound(), Some(b), "[H19:adopted]");
}

#[test]
fn a_resume_published_between_leaving_b_and_d_does_not_rewind_d() {
    let lane = Lane::new();
    for (channel, tmux, event) in [(7_720, "n2b-h20", "resume"), (7_721, "n2b-h20u", "prompt")] {
        let (b, c, d) = (uuid(), uuid(), uuid());
        let pane = Pane::new(&lane, channel, tmux);
        pane.chain(&[(&b, 10), (&c, 20), (&d, 40)]);
        shift_mtime(&pane.path(&b), 3600);
        let rotation = claude_session_rotation_for_tmux(tmux);
        let hook = match event {
            "resume" => start("resume", &pane.path(&b), 30),
            _ => prompt(&pane.path(&b), 30),
        };
        assert_eq!(pane.send(&b, hook), REGRESSION, "[H20:kept_d] {event}");
        assert_eq!(pane.bound(), Some(d), "[H20:kept_d] {event}");
        let refused = [format!("source:{b}"), format!("rejected:{b}:regression")];
        assert_eq!(pane.naming(&b), refused, "[H20:kept_d] {event}");
        assert_eq!(
            claude_session_rotation_for_tmux(tmux),
            rotation,
            "[H20:kept_d]"
        );
    }
}

#[test]
fn a_resume_published_before_the_waiting_session_does_not_return() {
    let lane = Lane::new();
    let (b, c, d, e) = (uuid(), uuid(), uuid(), uuid());
    let pane = Pane::new(&lane, 7_722, "n2b-h20a");
    pane.chain(&[(&b, 10), (&c, 20), (&d, 25)]);
    assert_eq!(pane.send(&e, prompt(&pane.path(&e), 40)), PENDING);
    let resume = start("resume", &pane.path(&b), 30);
    assert_eq!(pane.send(&b, resume), REGRESSION, "[H20a:kept_d]");
    assert_eq!(pane.bound(), Some(d), "[H20a:kept_d]");
}

#[test]
fn a_resume_to_a_superseded_pending_is_a_new_source_not_its_resolution() {
    let lane = Lane::new();
    let (b, c) = (uuid(), uuid());
    let pane = Pane::new(&lane, 7_723, "n2b-h21");
    let a = pane.a.clone();
    assert_eq!(pane.send(&b, start("clear", &pane.path(&b), 10)), PENDING);
    assert_eq!(pane.send(&c, prompt(&pane.file(&c), 20)), ADOPTED);
    pane.file(&b);
    assert_eq!(pane.send(&b, start("resume", &pane.path(&b), 30)), ADOPTED);
    let log = pane.log();
    let tail: Vec<_> = log[log.len() - 3..].iter().map(kind).collect();
    let want = [
        format!("pending:{b}"),
        format!("source:{c}"),
        format!("source:{b}"),
    ];
    assert_eq!(tail, want, "[H21:kind]");
    let last = log.last().unwrap();
    assert_eq!(last.cause, BindingCause::Resume, "[H21:kind]");
    assert_eq!(
        last.old.as_ref().map(|o| o.session_id.clone()),
        Some(c.clone()),
        "[H21:kind]"
    );
    let history = pane.history();
    let current = history.current.clone().unwrap();
    assert_eq!(
        (current.session, current.since),
        (b.clone(), Some(at(30))),
        "[H21:fold]"
    );
    assert_eq!(history.left[&c].left_at, at(30), "[H21:fold]");
    assert_eq!(history.left[&a].left_at, at(20), "[H21:fold]");
    assert!(history.awaiting.is_none(), "[H21:fold]");
    forget_channel_for_tests(pane.channel);
    assert_eq!(pane.history(), history, "[H21:reload]");
    assert_eq!(
        pane.send(&c, stop(&pane.path(&c), 25)),
        REGRESSION,
        "[H21:late_c]"
    );
}

#[test]
fn a_registration_naming_another_session_keeps_the_waiting_pending() {
    let lane = Lane::new();
    let (b, c) = (uuid(), uuid());
    let pane = Pane::new(&lane, 7_724, "n2b-h21d");
    assert_eq!(pane.send(&b, start("clear", &pane.path(&b), 10)), PENDING);
    let (tmux, channel) = (pane.tmux, pane.channel);
    let c_binding = claude(&pane.file(&c), &c);
    assert!(register_rehydrated_tmux_runtime_binding(
        "claude", tmux, channel, c_binding
    ));
    assert_eq!(
        pane.history().awaiting.map(|w| w.session),
        Some(b.clone()),
        "[H21d:kept]"
    );
    pane.file(&b);
    assert_eq!(
        pane.send(&b, prompt(&pane.path(&b), 20)),
        RESOLVED,
        "[H21d:kept]"
    );
    assert_eq!(
        pane.kinds().pop(),
        Some(format!("resolved:{b}")),
        "[H21d:kept]"
    );
}

#[test]
fn an_old_log_resolution_of_a_superseded_pending_folds_at_its_own_time() {
    let lane = Lane::new();
    let (channel, tmux) = (7_725, "n2b-h21b");
    let nonce = stamp(tmux);
    let (a, b, c) = (uuid(), uuid(), uuid());
    let file = |session: &str| {
        let path = lane.touch(session);
        let (dev, ino) =
            crate::services::tui_o::shadow::capture::file_identity(&fs::metadata(&path).unwrap());
        let (session_id, path) = (session.to_owned(), path);
        SourceId {
            session_id,
            path,
            dev,
            ino,
        }
    };
    let (sa, sb, sc) = (file(&a), file(&b), file(&c));
    let line = |seq: u64, old: Option<&SourceId>, new: BindingTarget, secs: i64, sidecar: bool| {
        let event = BindingEvent {
            seq,
            channel_id: channel,
            provider: "claude".into(),
            tmux_session: tmux.into(),
            execution_nonce: Some(nonce.clone()),
            old: old.cloned(),
            new,
            cause: BindingCause::Unknown,
            parent_hint: None,
            evidence: crate::services::tui_prompt_dedupe::binding_events::BindingEvidence {
                hook_event: Some("user_prompt_submit".into()),
                received_at: at(secs),
            },
            committed_at: at(secs),
        };
        let mut value = serde_json::to_value(&event).unwrap();
        if sidecar {
            value["published_at"] = serde_json::to_value(at(secs)).unwrap();
        }
        value.to_string()
    };
    let pending = BindingTarget::Pending {
        payload_session_id: b.clone(),
        payload_transcript_path: Some(sb.path.display().to_string()),
    };
    let resolved = BindingTarget::Resolved {
        pending_seq: 2,
        source: sb.clone(),
    };
    let lines = [
        line(1, None, BindingTarget::Source(sa.clone()), 0, true),
        line(2, Some(&sa), pending, 10, true),
        line(3, Some(&sa), BindingTarget::Source(sc.clone()), 20, true),
        line(4, Some(&sc), resolved, 30, false),
    ];
    fs::create_dir_all(lane.log(channel).parent().unwrap()).unwrap();
    fs::write(lane.log(channel), lines.join("\n") + "\n").unwrap();
    register_tmux_channel(tmux, channel);
    register_provider_session("claude", &a, tmux);
    register_tmux_runtime_binding(tmux, claude(&sb.path, &b));
    let nonce = Some(nonce.as_str());
    let history = claude_history(channel, tmux, nonce).unwrap().1;
    let current = history.current.clone().unwrap();
    assert_eq!(
        (current.session, current.since),
        (b.clone(), Some(at(30))),
        "[H21b:fold]"
    );
    assert_eq!(history.left[&c].left_at, at(30), "[H21b:fold]");
    let late = adopt_from_hook(&a, &c, &prompt(&sc.path, 25));
    assert_eq!(late, REGRESSION, "[H21b:late_c]");
}

#[test]
fn a_late_confirm_of_the_bound_session_does_not_supersede_the_waiting_pending() {
    let lane = Lane::new();
    let b = uuid();
    let pane = Pane::new(&lane, 7_726, "n2b-h21c");
    // The pane is on A, registered without a hook and so not yet verified.
    let a = uuid();
    let (tmux, channel) = (pane.tmux, pane.channel);
    let a_path = pane.file(&a);
    assert!(register_rehydrated_tmux_runtime_binding(
        "claude",
        tmux,
        channel,
        claude(&a_path, &a)
    ));
    assert_eq!(pane.send(&b, start("clear", &pane.path(&b), 10)), PENDING);
    assert_eq!(pane.send(&a, stop(&a_path, 5)), ADOPTED);
    assert_eq!(
        pane.kinds().pop(),
        Some(format!("source:{a}")),
        "the late Stop repins A"
    );
    assert_eq!(
        pane.history().awaiting.map(|w| w.session),
        Some(b.clone()),
        "[H21c:kept]"
    );
    pane.file(&b);
    assert_eq!(
        pane.send(&b, prompt(&pane.path(&b), 20)),
        ADOPTED,
        "[H21c:resolved]"
    );
    let last = pane.log().pop().unwrap();
    assert_eq!(
        (kind(&last), last.cause),
        (format!("resolved:{b}"), BindingCause::Clear)
    );
}

#[test]
fn a_return_is_not_proven_while_the_binding_is_not_the_logged_current() {
    let lane = Lane::new();
    let (b, c, d) = (uuid(), uuid(), uuid());
    let pane = Pane::new(&lane, 7_727, "n2b-known");
    pane.chain(&[(&b, 10), (&c, 20)]);
    // The binding moves to D while no log can be written, so the log never names D.
    let d_path = pane.file(&d);
    set_test_root(None);
    register_tmux_runtime_binding(pane.tmux, claude(&d_path, &d));
    set_test_root(Some(lane.root.path()));
    assert!(pane.history().complete);
    assert_eq!(
        pane.send(&b, prompt(&pane.path(&b), 50)),
        CONFLICT,
        "[known:conflict]"
    );
    assert_eq!(pane.bound(), Some(d), "[known:conflict]");
}

#[test]
fn a_background_start_published_late_does_not_take_the_pane_back() {
    let lane = Lane::new();
    // An old /clear X whose start was queued after the pane moved on to Y.
    let (x, y) = (uuid(), uuid());
    let pane = Pane::new(&lane, 7_728, "n2b-u10a");
    pane.chain(&[(&y, 20)]);
    let unproven = AdoptionHttp::Skipped(AdoptSkip::SourceRejected(SourceRejection::UnprovenStart));
    let late = pane.send(&x, start("clear", &pane.file(&x), 30));
    assert_eq!(late, unproven, "[U10:clear]");
    retry_deferred_claude_adoptions();
    assert_eq!(
        pane.bound(),
        Some(y),
        "[U10:clear] nothing is left to retry"
    );
    // X's own prompt is evidence the pane is on X.
    assert_eq!(
        pane.send(&x, prompt(&pane.path(&x), 40)),
        ADOPTED,
        "[U10:clear]"
    );
    // A relaunch resuming B, whose own start is queued after the pane already took D.
    let (b, d) = (uuid(), uuid());
    let pane = Pane::new(&lane, 7_729, "n2b-u10b");
    pane.chain(&[(&b, 10)]);
    pane.relaunch(&b);
    register_provider_session("claude", &b, pane.tmux);
    let hook = prompt(&pane.file(&d), 40);
    assert_eq!(adopt_from_hook(&b, &d, &hook), ADOPTED);
    let payload = serde_json::json!({ "source": "resume", "transcript_path": pane.path(&b) });
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        RELAY_PUBLISHED_AT_HEADER,
        at(50).to_rfc3339().parse().unwrap(),
    );
    let late = observe_binding_hook(
        "claude",
        "SessionStart",
        Some(&b),
        Some(&b),
        &payload,
        &headers,
    );
    assert!(
        matches!(late, IngressOutcome::Proceed(_)),
        "[U10:resume] {late:?}"
    );
    assert_eq!(pane.bound(), Some(d), "[U10:resume]");
}
