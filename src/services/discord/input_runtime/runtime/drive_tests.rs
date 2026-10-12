#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, Instant};

use super::*;
use crate::services::tui_input::actor::{InputActor, Step};
use crate::services::tui_input::ledger::{Ledger, Record};
use crate::services::tui_input::rows::{AttemptEvidence, Entry, HeldReason, RowState};
use crate::services::tui_o::shadow::SourceBinding;
use crate::services::tui_o::shadow::binding_reader::source_id_for;
use crate::services::tui_o::writer::input_facts::{ChannelFact, TurnState};

const CHANNEL: u64 = 6_326_940;
const NONCE: &str = "runtime-drive-attempt";
const READY: &str = "────────────────────\n❯ \n────────────────────\n";

struct Fixture {
    dir: tempfile::TempDir,
    binding: SourceBinding,
    key: ViewKey,
}

impl Fixture {
    fn new(pause: &str) -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/i01-tmp");
        fs::create_dir_all(&root).unwrap();
        let dir = tempfile::tempdir_in(root).unwrap();
        fs::write(dir.path().join("screen"), READY).unwrap();
        fs::write(dir.path().join(format!("pause-{pause}")), b"").unwrap();
        let transcript = dir.path().join("source.jsonl");
        fs::write(&transcript, "{\"type\":\"summary\"}\n").unwrap();
        let source = source_id_for("runtime-drive-source", &transcript).unwrap();
        let binding = SourceBinding {
            channel_id: CHANNEL,
            provider: ShadowProvider::Claude,
            source: source.clone(),
        };
        let key = ViewKey {
            provider: ShadowProvider::Claude,
            source,
            tmux_session: format!("g2b-runtime-{}", uuid::Uuid::new_v4()),
            execution_nonce: NONCE.into(),
        };
        let script = r#"#!/bin/sh
set -eu
root="${0%/*}"
cd "$root"
if [ "$1" = -u ]; then shift; fi
printf '%s\n' "$1" >> log
pause() { touch paused; while [ ! -f resume ]; do sleep 0.01; done; }
case "$1" in
has-session) ;;
load-buffer)
    for last do :; done
    cp "$last" buffer
    if [ -f pause-load ]; then pause; fi
    ;;
paste-buffer)
    { printf '────────────────────\n❯ '; cat buffer; printf '\n────────────────────\n'; } > screen
    touch pasted
    ;;
capture-pane)
    if [ -f pasted ] && [ -f pause-capture ]; then pause; fi
    cat screen
    ;;
send-keys) ;;
*) exit 9 ;;
esac
"#;
        let tmux = dir.path().join("tmux");
        fs::write(&tmux, script).unwrap();
        fs::set_permissions(tmux, fs::Permissions::from_mode(0o755)).unwrap();
        Self { dir, binding, key }
    }

    fn ledger(&self) -> Ledger {
        let mut ledger = Ledger::open(&self.dir.path().join("runtime"), CHANNEL).unwrap();
        ledger
            .append_entry(
                &Entry::Received {
                    key: 1,
                    input: serde_json::json!({"text":"hello"}),
                },
                &[],
            )
            .unwrap();
        ledger
    }

    fn calls(&self, command: &str) -> usize {
        fs::read_to_string(self.dir.path().join("log"))
            .unwrap_or_default()
            .lines()
            .filter(|line| *line == command)
            .count()
    }

    fn durable_attempt(&self) -> AttemptEvidence {
        let path =
            crate::services::tui_input::ledger::dir(&self.dir.path().join("runtime"), CHANNEL)
                .join("wal.0.jsonl");
        let records: Vec<Record> = fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let intent = records
            .iter()
            .find(|record| {
                record.kind == "transition" && record.payload["state"]["state"] == "injecting"
            })
            .expect("Injecting must be durable before the native effect");
        let attempt: AttemptEvidence =
            serde_json::from_value(intent.payload["attempt"].clone()).unwrap();
        assert_eq!(attempt.binding, self.binding);
        assert_eq!(attempt.execution_nonce, NONCE);
        assert_eq!(attempt.source_ids, [1]);
        assert!(attempt.rendered_prompt.contains("hello"));
        attempt
    }
}

#[test]
fn b_runtime_drive_actual_pane_keeps_wal_attempt_and_epoch_at_both_spawns() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    for pause in ["load", "capture"] {
        let fixture = Fixture::new(pause);
        let _path = crate::config::TestEnvVarGuard::prepend_path_after_shared_test_env_lock(
            fixture.dir.path(),
        );
        let ownership = Arc::new(OwnershipGate::default());
        ownership.acquired();
        let mut drive = RuntimeDrive::new(
            ProviderKind::Claude,
            fixture.key.tmux_session.clone(),
            Arc::new(Http::new("fixture-token")),
            CHANNEL,
            Arc::clone(&ownership),
        );
        let mut wrong = fixture.key.clone();
        wrong.provider = ShadowProvider::Codex;
        assert!(drive.pane(&wrong).is_none());
        wrong = fixture.key.clone();
        wrong.tmux_session.push_str("-other");
        assert!(drive.pane(&wrong).is_none());
        wrong = fixture.key.clone();
        wrong.execution_nonce.clear();
        assert!(drive.pane(&wrong).is_none());
        assert_eq!(fixture.calls("has-session"), 0);
        let mut pane = drive
            .pane(&fixture.key)
            .expect("actual adapter must build its pane");
        pane.attest_test_nonce(NONCE);
        assert_eq!(fixture.calls("has-session"), 1);
        let mut actor = InputActor::new(fixture.binding.clone(), pane);
        let mut ledger = fixture.ledger();
        let fact = ChannelFact {
            binding: fixture.binding.clone(),
            through: fs::metadata(&fixture.binding.source.path).unwrap().len(),
            state: TurnState::Idle,
        };
        let (step, ledger, durable) = std::thread::scope(|scope| {
            let worker = scope.spawn(move || {
                let step = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(actor.step(&mut ledger, Some(&fact), Instant::now()))
                    .unwrap();
                (step, ledger)
            });
            let until = Instant::now() + Duration::from_secs(22);
            while !fixture.dir.path().join("paused").exists() {
                assert!(
                    !worker.is_finished() && Instant::now() < until,
                    "actual runtime pane did not reach {pause}"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            let durable = fixture.durable_attempt();
            ownership.lost();
            fs::write(fixture.dir.path().join("resume"), b"").unwrap();
            let (step, ledger) = worker.join().unwrap();
            (step, ledger, durable)
        });
        assert_eq!(fixture.calls("load-buffer"), 1);
        assert_eq!(
            fixture.calls("send-keys"),
            0,
            "{pause}: loss must veto actual Enter spawn"
        );
        let expected = if pause == "load" {
            assert_eq!(
                fixture.calls("paste-buffer"),
                0,
                "loss must veto actual paste spawn"
            );
            RowState::Ready
        } else {
            assert_eq!(fixture.calls("paste-buffer"), 1);
            assert!(
                fs::read_to_string(fixture.dir.path().join("screen"))
                    .unwrap()
                    .contains("hello")
            );
            RowState::Held(HeldReason::Ambiguous)
        };
        assert_eq!(step, Step::Moved(1, expected));
        let rows = ledger.rows().unwrap();
        let row = rows.row(1).unwrap();
        assert_eq!(row.state, expected);
        assert_eq!(row.attempt.as_ref(), Some(&durable));
        assert_eq!(row.received_seq, Some(1));
    }
}
