//! A Herdr resume after a host reboot renumbered the pinned transcript's device: the resumed start
//! is logged under the new execution and answered; only an earlier boot's pin may be renumbered.

use super::*;
use crate::services::claude_tui::source_verify::boot_time_for_tests;
use crate::services::tui_prompt_dedupe::binding_context::{
    BindingContext, CapturedContext, HookBindingEnvelope, PreparedIncarnation,
};
use crate::services::tui_prompt_dedupe::binding_events::{
    BindingCause, CauseSource, Committed, Proposal, SourceId, forget_channel_for_tests,
    pinned_source, record_rejected, record_verified,
};
use crate::services::tui_prompt_dedupe::{
    AFTER_CHECK, admit_herdr_execution, install_herdr_execution,
    register_launched_tmux_runtime_binding, release_herdr_execution,
};
use chrono::{DateTime, Utc};
use std::os::unix::fs::MetadataExt;

/// Channel `CHANNEL`'s pane, its session S, and the Herdr execution A that pinned S.
struct Reboot {
    ingress: Ingress,
    tmux: String,
    s: String,
    path: PathBuf,
    a: String,
    _context: (tempfile::TempDir, [crate::config::TestEnvVarGuard; 2]),
}

const CHANNEL: u64 = 7_641;

impl Reboot {
    /// Execution A launches S and a check pins it verified, as a bound Herdr pane's log holds it.
    fn bound() -> Self {
        let context = crate::services::tui_prompt_dedupe::binding_context::tests::fixture();
        let ingress = Ingress::new();
        let (tmux, s) = (format!("renumber-resume-{}", uuid()), uuid());
        let path = ingress.transcript(&s);
        register_tmux_channel(&tmux, CHANNEL);
        register_provider_session("claude", &s, &tmux);
        let mut reboot = Self {
            ingress,
            tmux,
            s,
            path,
            a: String::new(),
            _context: context,
        };
        reboot.a = reboot.launch("fresh").execution_nonce;
        let path = reboot.path.display().to_string();
        let proposal = Proposal {
            channel_id: CHANNEL,
            provider: "claude",
            tmux_session: &reboot.tmux,
            session_id: Some(&reboot.s),
            path: &path,
            replaced: None,
            cause: CauseSource::Observed,
            hook: None,
        };
        let pinned = record_verified(&proposal, &reboot.identity()).unwrap();
        assert_eq!(pinned, Committed::Appended);
        assert_eq!(reboot.pin(), Some(reboot.identity()), "A pinned S verified");
        reboot
    }

    /// A Herdr launch of S in `mode` on the pane: its context, nonce marker, listing and binding.
    fn launch(&self, mode: &str) -> BindingContext {
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        let context = BindingContext {
            schema: 1,
            provider: "claude".into(),
            created_at: Utc::now(),
            execution_nonce: nonce.clone(),
            tmux_session: self.tmux.clone(),
            channel_id: Some(CHANNEL),
            owner_runtime_root: crate::services::tmux_common::current_tmux_owner_marker(),
            host: None,
            expected_native_session_id: Some(self.s.clone()),
            launch_mode: mode.into(),
            provider_root: None,
            first_prompt_digest: None,
            source_policy: None,
        };
        PreparedIncarnation::create(context.clone()).unwrap();
        let marker = crate::services::tmux_common::session_temp_path(&self.tmux, "spawn_nonce");
        fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
        fs::write(marker, &nonce).unwrap();
        install_herdr_execution(&self.tmux, &nonce);
        admit_herdr_execution(&self.tmux, &nonce);
        register_launched_tmux_runtime_binding(&self.tmux, claude(&self.path, &self.s));
        context
    }

    fn hook(&self, event: &str, payload: &Value, context: Option<&BindingContext>) -> u16 {
        let envelope = context.map(|context| {
            let context = CapturedContext::Captured(context.clone());
            let observed = Default::default();
            HookBindingEnvelope { context, observed }.encode().unwrap()
        });
        let uri = format!("/hooks/claude/{event}?session_id={}", self.s);
        let id = uuid();
        (self.ingress)
            .send_envelope(&uri, payload, Some(&id), envelope.as_deref())
            .0
    }

    /// B's resumed SessionStart of S, as its relay sends it.
    fn resumed_start(&self, b: &BindingContext) -> u16 {
        let payload = self.ingress.payload(&self.s, Some("resume"));
        self.hook("SessionStart", &payload, Some(b))
    }

    /// S's transcript as a stat finds it now.
    fn identity(&self) -> SourceId {
        identity(&self.s, &self.path)
    }

    fn pin(&self) -> Option<SourceId> {
        pinned_source(CHANNEL, &self.tmux).unwrap()
    }

    /// A reboot as the log sees it: every record of S names another device, and the process
    /// forgets the channel. Returns the boot time that follows every record so far.
    fn renumber(&self) -> DateTime<Utc> {
        reboot(&self.ingress, self.identity().dev)
    }
}

/// `session`'s transcript at `path` as a stat finds it now.
fn identity(session: &str, path: &std::path::Path) -> SourceId {
    let meta = fs::metadata(path).unwrap();
    let (session_id, path) = (session.to_owned(), path.to_path_buf());
    let (dev, ino) = (meta.dev(), meta.ino());
    SourceId {
        session_id,
        path,
        dev,
        ino,
    }
}

/// A reboot as `CHANNEL`'s log sees it: every record of device `dev` names another device, and the
/// process forgets the channel. Returns the boot time that follows every record so far.
fn reboot(ingress: &Ingress, dev: u64) -> DateTime<Utc> {
    let log = (ingress._root.path())
        .join(crate::services::tui_prompt_dedupe::binding_events::BINDING_EVENTS_DIR)
        .join(format!("{CHANNEL}.log"));
    let (real, moved) = (format!("\"dev\":{dev},"), format!("\"dev\":{},", dev + 1));
    let body = fs::read_to_string(&log).unwrap();
    assert!(body.contains(&real), "{body}");
    fs::write(&log, body.replace(&real, &moved)).unwrap();
    forget_channel_for_tests(CHANNEL);
    std::thread::sleep(std::time::Duration::from_millis(2));
    let boot = Utc::now();
    std::thread::sleep(std::time::Duration::from_millis(2));
    boot
}

#[test]
fn a_herdr_resume_after_a_reboot_renumbered_the_pinned_transcript_is_logged_and_answered() {
    let pane = Reboot::bound();
    let boot = pane.renumber();
    let _boot = boot_time_for_tests(Some(boot));
    release_herdr_execution(&pane.tmux, &pane.a);
    let b = pane.launch("resume");
    assert_eq!(pane.resumed_start(&b), 202, "B's resumed start is answered");
    let log = events(CHANNEL);
    let start = log.last().unwrap();
    assert_eq!(
        start.execution_nonce.as_deref(),
        Some(b.execution_nonce.as_str())
    );
    assert_eq!(start.cause, BindingCause::Resume);
    assert_eq!(start.new, BindingTarget::Source(pane.identity()));
    assert_eq!(pane.pin(), Some(pane.identity()), "re-pinned verified");
    let bound = runtime_binding_for_tmux_session(&pane.tmux).and_then(|b| b.session_id);
    assert_eq!(
        bound.as_deref(),
        Some(pane.s.as_str()),
        "the pane is attached to S"
    );

    assert_eq!(pane.resumed_start(&b), 202, "a resend");
    assert_eq!(events(CHANNEL).len(), log.len(), "no second re-pin");
    forget_channel_for_tests(CHANNEL);
    assert_eq!(
        pane.pin(),
        Some(pane.identity()),
        "the re-pin is in the log"
    );
}

#[test]
fn a_device_change_after_this_boot_or_without_a_boot_time_stays_refused() {
    let pane = Reboot::bound();
    let boot = pane.renumber();
    // The pin's record was committed after the boot: the same boot changed the device.
    let _boot = boot_time_for_tests(Some(boot - chrono::Duration::hours(1)));
    let b = pane.launch("resume");
    assert_eq!(pane.resumed_start(&b), 425, "same boot");
    drop(_boot);
    let _unknown = boot_time_for_tests(None);
    assert_eq!(pane.resumed_start(&b), 425, "boot time unknown");
    let _boot = boot_time_for_tests(Some(boot));
    assert_eq!(pane.resumed_start(&b), 202, "an earlier boot's pin");

    // A second device change after the re-pin is in the boot that re-pinned it.
    let repinned = pane.pin().unwrap();
    pane.renumber();
    assert_eq!(
        pane.resumed_start(&b),
        425,
        "renumbered again in the same boot"
    );
    let moved = SourceId {
        dev: repinned.dev + 1,
        ..repinned
    };
    assert_eq!(pane.pin(), Some(moved), "the pin stays");
}

#[test]
fn a_reboot_does_not_admit_another_inode_or_another_sessions_first_record() {
    for case in ["replaced", "removed", "foreign"] {
        let pane = Reboot::bound();
        let boot = pane.renumber();
        let _boot = boot_time_for_tests(Some(boot));
        match case {
            "replaced" => {
                let staged = pane.path.with_extension("staged");
                fs::write(&staged, fs::read(&pane.path).unwrap()).unwrap();
                fs::rename(&staged, &pane.path).unwrap();
            }
            "removed" => fs::remove_file(&pane.path).unwrap(),
            _ => {
                let row = json!({"type": "mode", "sessionId": uuid()});
                fs::write(&pane.path, format!("{row}\n")).unwrap();
            }
        }
        let b = pane.launch("resume");
        let pinned = pane.pin();
        assert_eq!(pane.resumed_start(&b), 425, "{case}");
        assert_eq!(pane.pin(), pinned, "{case}: the pin stays");
    }
}

#[test]
fn a_retire_without_a_device_change_resumes_as_before() {
    let pane = Reboot::bound();
    let _boot = boot_time_for_tests(Some(Utc::now()));
    release_herdr_execution(&pane.tmux, &pane.a);
    let b = pane.launch("resume");
    assert_eq!(pane.resumed_start(&b), 202);
    let start = events(CHANNEL).last().cloned().unwrap();
    assert_eq!(
        start.execution_nonce.as_deref(),
        Some(b.execution_nonce.as_str())
    );
}

#[test]
fn a_record_after_the_boot_does_not_date_the_earlier_pin() {
    let pane = Reboot::bound();
    let boot = pane.renumber();
    let _boot = boot_time_for_tests(Some(boot));
    // An audit after the boot is the latest record, but the pin is still the earlier boot's.
    let x = uuid();
    let x_path = pane.ingress.path(&x).display().to_string();
    let proposal = Proposal {
        channel_id: CHANNEL,
        provider: "claude",
        tmux_session: &pane.tmux,
        session_id: Some(&x),
        path: &x_path,
        replaced: None,
        cause: CauseSource::Observed,
        hook: None,
    };
    assert!(record_rejected(&proposal, "after the boot").unwrap());
    let b = pane.launch("resume");
    assert_eq!(pane.resumed_start(&b), 202);
    assert_eq!(pane.pin(), Some(pane.identity()));
}

#[test]
fn a_file_swapped_in_after_the_renumber_check_is_not_pinned() {
    let pane = Reboot::bound();
    let boot = pane.renumber();
    // B's launch cannot re-pin without a boot time, so its resumed start is the first re-pin.
    let _unknown = boot_time_for_tests(None);
    let b = pane.launch("resume");
    let _boot = boot_time_for_tests(Some(boot));
    let (pinned, lines) = (pane.pin(), events(CHANNEL).len());
    let (path, staged) = (pane.path.clone(), pane.path.with_extension("staged"));
    fs::write(&staged, fs::read(&path).unwrap()).unwrap();
    let swap = move || fs::rename(&staged, &path).unwrap();
    AFTER_CHECK.with_borrow_mut(|after| *after = Some(Box::new(swap)));
    assert_eq!(
        pane.resumed_start(&b),
        425,
        "the path names another file now"
    );
    assert!(
        AFTER_CHECK.with_borrow(Option::is_none),
        "the swap ran inside the re-pin"
    );
    assert_eq!(pane.pin(), pinned, "the pin stays");
    assert_eq!(events(CHANNEL).len(), lines, "nothing logged");
}

/// L → A (pinned) → reboot → B → resume A, so A's pin precedes the boot and its leaving follows it:
/// A comes back on its own pin's time, re-pinned under the new device.
#[test]
fn a_session_left_after_the_reboot_returns_on_its_earlier_boots_pin() {
    let _context = crate::services::tui_prompt_dedupe::binding_context::tests::fixture();
    let ingress = Ingress::new();
    let (tmux, l, a, b) = (
        format!("renumber-return-{}", uuid()),
        uuid(),
        uuid(),
        uuid(),
    );
    let marker = crate::services::tmux_common::session_temp_path(&tmux, "spawn_nonce");
    fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
    fs::write(marker, uuid::Uuid::new_v4().simple().to_string()).unwrap();
    ingress.pane(&tmux, CHANNEL, &l);
    let a_path = ingress.transcript(&a);
    let switch = |session: &str, source: Option<&str>| {
        let payload = ingress.payload(session, source);
        let event = if source.is_some() {
            "SessionStart"
        } else {
            "UserPromptSubmit"
        };
        let status = ingress.claude_hook(event, &l, &payload, Some(&uuid()));
        clear_claude_session_rotation(&tmux);
        status
    };
    let bound = || runtime_binding_for_tmux_session(&tmux).and_then(|b| b.session_id);
    let pin = || pinned_source(CHANNEL, &tmux).unwrap();
    assert_eq!(switch(&a, None), 202, "L → A");
    assert_eq!(pin(), Some(identity(&a, &a_path)), "A pinned verified");
    let boot = reboot(&ingress, identity(&a, &a_path).dev);
    let _boot = boot_time_for_tests(Some(boot));
    ingress.transcript(&b);
    assert_eq!(switch(&b, None), 202, "A → B after the boot");
    assert_eq!(bound().as_deref(), Some(b.as_str()));
    assert_eq!(switch(&a, Some("resume")), 202, "resume A");
    assert_eq!(bound().as_deref(), Some(a.as_str()), "A is bound again");
    assert_eq!(pin(), Some(identity(&a, &a_path)), "re-pinned verified");
    forget_channel_for_tests(CHANNEL);
    assert_eq!(
        pin(),
        Some(identity(&a, &a_path)),
        "the re-pin is in the log"
    );
}
