//! Activation: a live Codex Herdr turn this process started settles `/stop` and a pane Escape by
//! its own terminal, with the Escape switch only in the live config and no settlement override.
use super::super::*;
use crate::services::provider::cancel_token_claude_interrupt::{
    HERDR_CANCEL_OVERRIDE, herdr_cancel_enabled,
};
use crate::services::provider::herdr_before_start::{ExitDecision, seal_exit};

/// The live config with the Escape switch on, restored when dropped.
struct ConfiguredSwitch(crate::config::Config);

impl ConfiguredSwitch {
    fn on() -> Self {
        let previous = crate::config_live_reload::current()
            .map(|config| (*config).clone())
            .unwrap_or_default();
        let mut live = previous.clone();
        live.runtime.herdr_cancel_enabled = Some(true);
        crate::config_live_reload::install(live);
        Self(previous)
    }
}

impl Drop for ConfiguredSwitch {
    fn drop(&mut self) {
        crate::config_live_reload::install(self.0.clone());
    }
}

/// `/stop` through the production entry, where only the live config can enable the Escape.
fn configured_stop(fx: &Fixture, shared: &Arc<crate::services::discord::SharedData>) -> String {
    use crate::services::discord::turn_bridge::{CommandStop, begin_user_stop};
    assert_eq!(HERDR_CANCEL_OVERRIDE.with(std::cell::Cell::get), None);
    assert!(
        herdr_cancel_enabled(),
        "the configured switch alone enables it"
    );
    let channel = poise::serenity_prelude::ChannelId::new(CHANNEL);
    let stop = begin_user_stop(shared, &ProviderKind::Codex, channel, true, "/stop");
    match fx.rt.block_on(stop) {
        CommandStop::Herdr(stop) => format!("{stop:?}"),
        _ => "not a Herdr stop".into(),
    }
}

/// What a live turn showed before its terminal.
#[derive(Debug, PartialEq)]
struct Live {
    stop: Option<String>,
    intent: bool,
    escapes: usize,
    exit: ExitDecision,
    owns_mailbox: bool,
}

/// A cold Codex turn bound to its own rollout, optionally stopped by `/stop` before its own start;
/// its provider then aborts. Returns what it showed, its result, frames and rollout.
fn aborted_turn(
    tag: &str,
    stopped: bool,
) -> (
    Fixture,
    Live,
    Result<(), String>,
    Vec<StreamMessage>,
    PathBuf,
) {
    let fx = Fixture::admitted(tag);
    let _switch = ConfiguredSwitch::on();
    let launcher = Arc::new(Launcher::default());
    let shared = stop_runtime(&fx);
    let seen = Mutex::new(None);
    let (result, frames) = fx.turn(&HostedRecord::Legacy, &fx.ports(&launcher), || {
        let Some(nonce) = fx.start_provider(&launcher, true) else {
            return;
        };
        if !wait_for(&fx.finished, "the prompt", || fx.rig.sends().len() == 2) {
            return;
        }
        let token = fx.cancel.lock().unwrap().clone();
        mailbox_turn(&fx, &shared, token.clone());
        fx.rig.answer("pane.read", screen(WORKING));
        let stop = stopped.then(|| configured_stop(&fx, &shared));
        let session = uuid::Uuid::new_v4().to_string();
        let path = fx.rollout(&session);
        assert_eq!(fx.session_start(&context_of(&nonce), &session, &path), 202);
        append(&path, &started_lines("t1", "답"));
        let escapes = settle_escapes(&fx, usize::from(stopped));
        let channel = poise::serenity_prelude::ChannelId::new(CHANNEL);
        let current = fx
            .rt
            .block_on(shared.mailbox(channel).snapshot())
            .cancel_token;
        let state = token
            .herdr_interrupt_state()
            .expect("the live turn's own state");
        let live = Live {
            stop,
            intent: state.user_stop.load(Ordering::SeqCst),
            escapes,
            exit: seal_exit(&token, false, false),
            owns_mailbox: current.is_some_and(|current| Arc::ptr_eq(&current, &token)),
        };
        let aborted = json!({"type": "event_msg", "payload": {"type": "turn_aborted",
            "turn_id": "t1"}});
        append(&path, &[aborted]);
        *seen.lock().unwrap() = Some((live, path));
    });
    let (live, path) = seen
        .into_inner()
        .unwrap()
        .expect("the turn reached its own start");
    (fx, live, result, frames, path)
}

fn own_aborts(frames: &[StreamMessage]) -> usize {
    let aborted = |frame: &&StreamMessage| {
        matches!(frame, StreamMessage::CodexTuiTerminalDone { kind, .. }
            if *kind == crate::services::agent_protocol::NativeTerminalKind::Aborted)
    };
    frames.iter().filter(aborted).count()
}

// A configured `/stop` records its intent and sends one Escape at the turn's own start; the turn
// keeps its token and holds until its own Aborted, and the next prompt is then written once.
#[test]
fn a_configured_stop_settles_a_live_turn_on_its_own_abort_pg() {
    let (fx, live, result, frames, path) = aborted_turn("activation-stop", true);
    let expected = Live {
        stop: Some("Requested(NotSent(Pending))".into()),
        intent: true,
        escapes: 1,
        exit: ExitDecision::Hold,
        owns_mailbox: true,
    };
    assert_eq!(live, expected);
    assert_eq!(result, Ok(()));
    assert_eq!(own_aborts(&frames), 1, "{frames:?}");
    let escape = json!({"pane_id": PANE, "keys": ["esc"]});
    assert_eq!(
        fx.rig.sends(),
        [prompt_sends(), vec![escape.clone()]].concat()
    );
    fx.rig.answer("pane.read", screen(READY));
    let launcher = Arc::new(Launcher::default());
    let (next, messages) = fx.turn(&fx.record(), &fx.ports(&launcher), || {
        reply(&fx, &path, 5, "t2", "둘째")
    });
    assert_eq!(next, Ok(()));
    assert_eq!(texts(&messages), ["둘째"]);
    let sent = [prompt_sends(), vec![escape], prompt_sends()].concat();
    assert_eq!(
        fx.rig.sends(),
        sent,
        "one Escape, then the next prompt once"
    );
    // The same turn restored after a restart carries no state here: its exit stays the existing one.
    let nonce = fx.cancel.lock().unwrap().turn_nonce().map(str::to_owned);
    let restored = CancelToken::from_persisted_turn_nonce(nonce);
    assert_eq!(seal_exit(&restored, false, false), ExitDecision::Normal);
}

// An Escape typed in the pane, with no stop intent, ends the live turn on its own Aborted and
// sends no key of ours.
#[test]
fn a_pane_escape_settles_a_live_turn_on_its_own_abort_pg() {
    let (fx, live, result, frames, _) = aborted_turn("activation-pane", false);
    let expected = Live {
        stop: None,
        intent: false,
        escapes: 0,
        exit: ExitDecision::Hold,
        owns_mailbox: true,
    };
    assert_eq!(live, expected);
    assert_eq!(result, Ok(()));
    assert_eq!(own_aborts(&frames), 1, "{frames:?}");
    assert_eq!(fx.rig.sends(), prompt_sends());
}
