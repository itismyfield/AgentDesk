use super::*;
use crate::services::claude::herdr_turn::composer;
use crate::services::claude_tui::host_input::{InputRefusal, InputRun};
use crate::services::tui_prompt_dedupe::binding_events as events;
use std::cell::{Cell, RefCell};
use std::os::unix::fs::MetadataExt;
use std::rc::Rc;

thread_local! {
    static AFTER_RESERVE: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
}
pub(crate) fn after_reserve() {
    if let Some(callback) = AFTER_RESERVE.with(|slot| slot.take()) {
        callback();
    }
}

struct ClearRig {
    rig: Rig,
    channel: u64,
    previous_root: Option<PathBuf>,
}
impl ClearRig {
    fn new() -> Self {
        let channel = 8_080_992;
        let rig = Rig::for_channel(Some(channel));
        rig.assistant("previous answer");
        rig.terminal();
        let previous_root = events::test_root();
        events::set_test_root(Some(rig._dir.path()));
        let r = Self {
            rig,
            channel,
            previous_root,
        };
        r.log_source(channel);
        r
    }
    fn path(&self, native: &str) -> PathBuf {
        self.rig.path.with_file_name(format!("{native}.jsonl"))
    }
    fn log_source(&self, channel: u64) {
        let native = self.rig.ctx.expected_native_session_id.as_deref().unwrap();
        let path = self.rig.path.to_str().unwrap();
        let payload = json!({"source":"startup","session_id":native,"transcript_path":path});
        let hook = events::HookSignal::from_payload("session_start", &payload);
        let meta = fs::metadata(path).unwrap();
        events::record_verified(
            &events::Proposal {
                channel_id: channel,
                provider: "claude",
                tmux_session: &self.rig.session,
                session_id: Some(native),
                path,
                replaced: None,
                cause: events::CauseSource::Hook(events::BindingCause::Startup),
                hook: Some(&hook),
            },
            &events::SourceId {
                session_id: native.into(),
                path: path.into(),
                dev: meta.dev(),
                ino: meta.ino(),
            },
        )
        .unwrap();
    }
    fn pending(&self, native: &str, channel: u64, path: &Path) {
        let text = path.to_str().unwrap();
        let payload = json!({"source":"clear","session_id":native,"transcript_path":text});
        let hook = events::HookSignal::from_payload("session_start", &payload);
        events::record_pending(&events::Proposal {
            channel_id: channel,
            provider: "claude",
            tmux_session: &self.rig.session,
            session_id: Some(native),
            path: text,
            replaced: None,
            cause: events::CauseSource::Hook(events::BindingCause::Clear),
            hook: Some(&hook),
        })
        .unwrap();
        let records = events::binding_events_since(channel, 0).unwrap();
        let last = records.last().unwrap();
        assert_eq!(last.channel_id, channel);
        assert!(matches!(&last.new, events::BindingTarget::Pending {
            payload_session_id, payload_transcript_path
        } if payload_session_id == native && payload_transcript_path.as_deref() == Some(text)));
    }
    fn start(&self, native: &str, source: &str) {
        note_hook(
            "claude",
            &HookEventKind::SessionStart,
            &self.rig.session,
            &json!({"source":source,"session_id":native,"transcript_path":self.path(native)}),
            Some(&self.rig.ctx),
            Some(Utc::now()),
            false,
        );
    }
    fn run(&self, native: &str, sent: &Cell<usize>) -> InputRun {
        composer::when_idle(
            &self.rig.session,
            &self.path(native),
            || {
                Some(
                    include_str!(concat!(
                        env!("CARGO_MANIFEST_DIR"),
                        "/tests/fixtures/tui_input/claude-2.1.293-idle.txt"
                    ))
                    .into(),
                )
            },
            None,
            || {
                assert!(!self.path(native).exists());
                sent.set(sent.get() + 1);
                InputRun::Applied
            },
        )
    }
    fn closed(&self, native: &str) {
        let sent = Cell::new(0);
        let result = self.run(native, &sent);
        assert_eq!(sent.get(), 0, "no payload or Enter for an unproved clear");
        assert!(
            matches!(result, InputRun::Refused(InputRefusal::Unknown)),
            "{result:?}"
        );
    }
}
impl Drop for ClearRig {
    fn drop(&mut self) {
        events::set_test_root(self.previous_root.as_deref());
    }
}

#[test]
fn latest_pending_clear_submits_once_before_file_and_preserves_old_binding() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    for count in [1, 2] {
        let r = ClearRig::new();
        let mut latest = String::new();
        for _ in 0..count {
            latest = uuid::Uuid::new_v4().to_string();
            r.pending(&latest, r.channel, &r.path(&latest));
            r.start(&latest, "clear");
        }
        let sent = Cell::new(0);
        assert!(matches!(r.run(&latest, &sent), InputRun::Applied));
        assert_eq!(sent.get(), 1);
        assert!(
            observe(&r.rig.session, Some(&r.path(&latest))).is_none(),
            "no replay"
        );
        let binding = tui_prompt_dedupe::runtime_binding_for_tmux_session(&r.rig.session).unwrap();
        assert_eq!(binding.output_path, r.rig.path.to_string_lossy());
        assert_eq!(binding.session_id, r.rig.ctx.expected_native_session_id);
    }
}

#[test]
fn pending_of_another_nonce_cannot_authorize_the_current_clear() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let r = ClearRig::new();
    let native = uuid::Uuid::new_v4().to_string();
    let marker = crate::services::tmux_common::session_temp_path(&r.rig.session, "spawn_nonce");
    let other = uuid::Uuid::new_v4().simple().to_string();
    let mut foreign = r.rig.ctx.clone();
    foreign.execution_nonce = other.clone();
    crate::services::tui_prompt_dedupe::binding_context::PreparedIncarnation::create(foreign)
        .unwrap();
    fs::write(&marker, &other).unwrap();
    r.pending(&native, r.channel, &r.path(&native));
    assert_eq!(
        events::binding_events_since(r.channel, 0)
            .unwrap()
            .last()
            .unwrap()
            .execution_nonce
            .as_deref(),
        Some(other.as_str())
    );
    fs::write(marker, &r.rig.ctx.execution_nonce).unwrap();
    r.start(&native, "clear");
    r.closed(&native);
}

#[test]
fn resume_signal_cannot_replace_the_clear_session_start() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let r = ClearRig::new();
    let native = uuid::Uuid::new_v4().to_string();
    r.pending(&native, r.channel, &r.path(&native));
    r.start(&native, "resume");
    r.closed(&native);
}

#[test]
fn clear_start_without_pending_or_stop_without_clear_start_sends_no_keys() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    for only_start in [true, false] {
        let r = ClearRig::new();
        let native = uuid::Uuid::new_v4().to_string();
        if only_start {
            r.start(&native, "clear");
        } else {
            r.pending(&native, r.channel, &r.path(&native));
            note_hook(
                "claude",
                &HookEventKind::Stop,
                &r.rig.session,
                &json!({"session_id":native,"transcript_path":r.path(&native)}),
                Some(&r.rig.ctx),
                Some(Utc::now()),
                false,
            );
        }
        r.closed(&native);
    }
}

#[test]
fn observing_an_unloaded_fold_never_repairs_its_unfinished_log() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let r = ClearRig::new();
    let native = uuid::Uuid::new_v4().to_string();
    r.pending(&native, r.channel, &r.path(&native));
    r.start(&native, "clear");
    assert!(observe(&r.rig.session, Some(&r.path(&native))).is_some());
    let log = r
        .rig
        ._dir
        .path()
        .join(events::BINDING_EVENTS_DIR)
        .join(format!("{}.log", r.channel));
    events::forget_channel_for_tests(r.channel);
    OpenOptions::new()
        .append(true)
        .open(&log)
        .unwrap()
        .write_all(b"{unfinished")
        .unwrap();
    let bytes = fs::read(&log).unwrap();
    r.closed(&native);
    assert_eq!(
        fs::read(log).unwrap(),
        bytes,
        "readiness cannot reload, truncate or sync a writer"
    );
}

#[test]
fn old_stop_and_old_clear_cannot_authorize_the_latest_pending() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let r = ClearRig::new();
    let old = uuid::Uuid::new_v4().to_string();
    let new = uuid::Uuid::new_v4().to_string();
    r.pending(&old, r.channel, &r.path(&old));
    r.start(&old, "clear");
    r.pending(&new, r.channel, &r.path(&new));
    note_hook(
        "claude",
        &HookEventKind::Stop,
        &r.rig.session,
        &json!({"session_id":new,"transcript_path":r.path(&new)}),
        Some(&r.rig.ctx),
        Some(Utc::now()),
        false,
    );
    r.closed(&old);
    r.closed(&new);
}

#[test]
fn another_channel_or_path_pending_cannot_open_the_new_source() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    for wrong_channel in [true, false] {
        let r = ClearRig::new();
        let native = uuid::Uuid::new_v4().to_string();
        if wrong_channel {
            r.log_source(r.channel + 1);
            r.pending(&native, r.channel + 1, &r.path(&native));
        } else {
            r.pending(
                &native,
                r.channel,
                &r.path(&uuid::Uuid::new_v4().to_string()),
            );
        }
        r.start(&native, "clear");
        r.closed(&native);
    }
}

#[test]
fn pending_seq_change_invalidates_both_observed_and_reserved_proofs() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let r = Rc::new(ClearRig::new());
    let native = uuid::Uuid::new_v4().to_string();
    r.pending(&native, r.channel, &r.path(&native));
    r.start(&native, "clear");
    let proof = observe(&r.rig.session, Some(&r.path(&native))).unwrap();
    let transient = uuid::Uuid::new_v4().to_string();
    r.pending(&transient, r.channel, &r.path(&transient));
    r.pending(&native, r.channel, &r.path(&native));
    assert!(
        reserve(proof).is_none(),
        "same native/path with a newer Pending seq"
    );
    r.start(&native, "clear");
    let after = r.clone();
    let changed = Rc::new(Cell::new(false));
    let check = changed.clone();
    let same = native.clone();
    AFTER_RESERVE.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move || {
            let last = uuid::Uuid::new_v4().to_string();
            after.pending(&last, after.channel, &after.path(&last));
            after.pending(&same, after.channel, &after.path(&same));
            check.set(true);
        }))
    });
    let sent = Cell::new(0);
    assert!(matches!(
        r.run(&native, &sent),
        InputRun::Refused(InputRefusal::Unknown)
    ));
    assert!(
        changed.get(),
        "the real caller reached reserve before Pending changed"
    );
    assert_eq!(sent.get(), 0);
}

#[test]
fn clear_known_non_delivery_releases_the_certificate_and_old_output_stays_fenced() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let r = ClearRig::new();
    let native = uuid::Uuid::new_v4().to_string();
    r.pending(&native, r.channel, &r.path(&native));
    r.start(&native, "clear");
    let old = tui_prompt_dedupe::runtime_binding_for_tmux_session(&r.rig.session).unwrap();
    let reservation = reserve(observe(&r.rig.session, Some(&r.path(&native))).unwrap()).unwrap();
    assert!(reservation.current());
    reservation.not_sent();
    r.rig.assistant("late old output");
    r.rig.terminal();
    assert!(observe(&r.rig.session, Some(&r.rig.path)).is_none());
    let sent = Cell::new(0);
    assert!(matches!(r.run(&native, &sent), InputRun::Applied));
    assert_eq!(sent.get(), 1);
    assert_eq!(
        tui_prompt_dedupe::runtime_binding_for_tmux_session(&r.rig.session),
        Some(old)
    );
    forget_session(&r.rig.session);
    r.start(&native, "clear");
    r.closed(&native);
    assert_eq!(
        sent.get(),
        1,
        "a sent clear certificate cannot bootstrap after restart"
    );
}

#[test]
fn clear_crash_after_reservation_cannot_reuse_the_authenticated_start() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let r = ClearRig::new();
    let native = uuid::Uuid::new_v4().to_string();
    r.pending(&native, r.channel, &r.path(&native));
    r.start(&native, "clear");
    let old = tui_prompt_dedupe::runtime_binding_for_tmux_session(&r.rig.session).unwrap();
    let reservation = reserve(observe(&r.rig.session, Some(&r.path(&native))).unwrap()).unwrap();
    assert!(reservation.current());
    drop(reservation);
    forget_session(&r.rig.session);
    r.start(&native, "clear");
    r.closed(&native);
    assert_eq!(
        tui_prompt_dedupe::runtime_binding_for_tmux_session(&r.rig.session),
        Some(old)
    );
}
