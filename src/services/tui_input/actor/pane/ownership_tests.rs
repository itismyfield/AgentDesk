#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::Instant;

use super::*;
use crate::services::discord::input_runtime::offer::Offer;
use crate::services::tui_o::ownership::OwnershipGate;

struct Fixture(tempfile::TempDir);

impl Fixture {
    fn new(pause: &str) -> Self {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/i01-tmp");
        fs::create_dir_all(&root).unwrap();
        let dir = tempfile::tempdir_in(root).unwrap();
        fs::write(
            dir.path().join("screen"),
            "────────────────────\n❯ \n────────────────────\n",
        )
        .unwrap();
        let script = format!(
            "#!/bin/sh\nset -e\ncd '{root}' || exit 9\necho \"$2\" >> log\n\
             pause() {{ touch paused; while [ ! -f resume ]; do sleep 0.01; done; }}\n\
             case \"$2\" in\n\
             load-buffer) for last do :; done; cp \"$last\" buffer; {load} ;;\n\
             paste-buffer) {{ printf '────────────────────\\n❯ '; cat buffer; printf '\\n────────────────────\\n'; }} > screen; touch pasted ;;\n\
             capture-pane) if [ -f pasted ]; then {capture}; fi; cat screen ;;\n\
             esac\nexit 0\n",
            root = dir.path().display(),
            load = if pause == "load" { "pause" } else { ":" },
            capture = if pause == "capture" { "pause" } else { ":" },
        );
        let program = dir.path().join("tmux");
        fs::write(&program, script).unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        Self(dir)
    }

    fn pane(&self, offer: Offer) -> TmuxPane {
        let mut pane = TmuxPane::with_program(
            &format!("epoch-{}", self.0.path().display()),
            self.0.path().join("tmux"),
            Duration::from_secs(5),
        )
        .with_offer(offer);
        pane.attest_test_nonce("epoch-test");
        pane
    }

    fn calls(&self, command: &str) -> usize {
        fs::read_to_string(self.0.path().join("log"))
            .unwrap_or_default()
            .lines()
            .filter(|line| *line == command)
            .count()
    }

    fn submit_during(&self, offer: Offer, change: impl FnOnce()) -> SendOutcome {
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| {
                let outcome = self.pane(offer).with_composer(|p| p.submit("hello"));
                fs::write(self.0.path().join("outcome"), format!("{outcome:?}")).unwrap();
                outcome.unwrap()
            });
            // Four bounded commands can precede the post-paste pause.
            let until = Instant::now() + Duration::from_secs(22);
            while !self.0.path().join("paused").exists() {
                assert!(
                    !worker.is_finished() && Instant::now() < until,
                    "effect seam did not pause: outcome={}, calls={}",
                    fs::read_to_string(self.0.path().join("outcome")).unwrap_or_default(),
                    fs::read_to_string(self.0.path().join("log")).unwrap_or_default()
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            change();
            fs::write(self.0.path().join("resume"), b"").unwrap();
            worker.join().unwrap()
        })
    }
}

#[test]
fn unknown_and_lost_never_create_an_offer_or_touch_tmux() {
    let gate = Arc::new(OwnershipGate::default());
    let fixture = Fixture::new("");
    for ownership in ["lost", "unknown"] {
        if ownership == "unknown" {
            gate.acquired();
            gate.uncertain();
        }
        let offer = Offer::begin(ShadowProvider::Claude, Arc::clone(&gate));
        let offered = offer.is_some();
        if let Some(offer) = offer {
            let _ = fixture
                .pane(offer)
                .with_composer(|pane| pane.submit("hello"));
        }
        assert!(!offered, "{ownership} must not create an offer");
        assert_eq!(fixture.calls("paste-buffer"), 0);
        assert_eq!(fixture.calls("send-keys"), 0);
    }
}

#[test]
fn ownership_changes_between_offer_and_paste_prevent_actual_spawn() {
    for state in ["unknown", "lost", "reacquired"] {
        let gate = Arc::new(OwnershipGate::default());
        gate.acquired();
        let offer = Offer::begin(ShadowProvider::Claude, Arc::clone(&gate)).unwrap();
        let fixture = Fixture::new("load");
        let outcome = fixture.submit_during(offer, || match state {
            "unknown" => gate.uncertain(),
            "lost" => gate.lost(),
            _ => {
                gate.acquired();
            }
        });
        assert!(
            matches!(outcome, SendOutcome::NotSent(_)),
            "{state}: {outcome:?}"
        );
        assert_eq!(fixture.calls("paste-buffer"), 0, "{state}");
        assert_eq!(fixture.calls("send-keys"), 0, "{state}");
    }
}

#[test]
fn ownership_changes_after_paste_preserve_draft_and_veto_enter() {
    for state in ["unknown", "lost", "reacquired"] {
        let gate = Arc::new(OwnershipGate::default());
        gate.acquired();
        let offer = Offer::begin(ShadowProvider::Claude, Arc::clone(&gate)).unwrap();
        let fixture = Fixture::new("capture");
        let outcome = fixture.submit_during(offer, || match state {
            "unknown" => gate.uncertain(),
            "lost" => gate.lost(),
            _ => {
                gate.acquired();
            }
        });
        assert!(
            matches!(outcome, SendOutcome::Indeterminate(_)),
            "{state}: {outcome:?}"
        );
        assert_eq!(fixture.calls("paste-buffer"), 1);
        assert_eq!(fixture.calls("send-keys"), 0, "{state}");
        assert!(
            fs::read_to_string(fixture.0.path().join("screen"))
                .unwrap()
                .contains("hello")
        );
    }
}

#[test]
fn reacquisition_requires_a_new_offer_before_submission() {
    let gate = Arc::new(OwnershipGate::default());
    gate.acquired();
    let old = Offer::begin(ShadowProvider::Claude, Arc::clone(&gate)).unwrap();
    gate.lost();
    gate.acquired();
    assert!(old.admit(|| ()).is_none());
    let new = Offer::begin(ShadowProvider::Claude, gate).unwrap();
    let fixture = Fixture::new("");
    let outcome = fixture
        .pane(new)
        .with_composer(|p| p.submit("hello"))
        .unwrap();
    assert_eq!(outcome, SendOutcome::Sent);
    assert_eq!(fixture.calls("paste-buffer"), 1);
    assert_eq!(fixture.calls("send-keys"), 1);
}

#[test]
fn an_offer_for_another_provider_cannot_spawn_paste_or_enter() {
    let gate = Arc::new(OwnershipGate::default());
    gate.acquired();
    let offer = Offer::begin(ShadowProvider::Codex, gate).unwrap();
    let fixture = Fixture::new("");
    let outcome = fixture
        .pane(offer)
        .with_composer(|p| p.submit("hello"))
        .unwrap();
    assert!(matches!(outcome, SendOutcome::NotSent(_)));
    assert_eq!(fixture.calls("paste-buffer"), 0);
    assert_eq!(fixture.calls("send-keys"), 0);
}
