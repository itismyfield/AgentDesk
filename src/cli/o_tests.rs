//! `agentdesk o status|resume` in a child process through the real subcommand parser and
//! dispatcher: status writes nothing, and only an approval the call wrote exits 0.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

use chrono::Utc;
use clap::Parser;

use crate::cli::args::Commands;
use crate::services::tui_o::shadow::{ShadowProvider, UnitKey, UnitKind};
use crate::services::tui_o::store::ledger::LedgerEntry;
use crate::services::tui_o::store::{Initialized, OStore, StoreConfig};

const CHILD: &str = "AGENTDESK_O_CLI_CHILD_ARGS";
const PAUSE: &str = "AGENTDESK_O_CLI_PAUSE_DIR";
const CHANNEL: u64 = 7;

#[derive(Parser)]
struct TestCli {
    #[command(subcommand)]
    command: Commands,
}

/// The exit code, stdout and stderr of `agentdesk <args>` run under `root`.
pub(crate) fn run_cli(root: &Path, args: &[&str]) -> (Option<i32>, String, String) {
    let Output {
        status,
        stdout,
        stderr,
    } = spawn(root, args).wait_with_output().unwrap();
    let text = |bytes| String::from_utf8(bytes).unwrap();
    (status.code(), text(stdout), text(stderr))
}

fn spawn(root: &Path, args: &[&str]) -> Child {
    command(root, args).spawn().unwrap()
}

fn command(root: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "cli::o::tests::o_cli_child", "--nocapture"])
        .args(["--test-threads=1"])
        .env("AGENTDESK_ROOT_DIR", root)
        .env(CHILD, serde_json::to_string(args).unwrap())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

/// In a child given a pause dir, `o resume` stops right before the store call until `go` exists.
pub(crate) fn pause_before_entry() {
    let Some(dir) = std::env::var_os(PAUSE).map(PathBuf::from) else {
        return;
    };
    std::fs::write(dir.join("paused"), b"").unwrap();
    wait_for(&dir.join("go"));
}

fn wait_for(path: &Path) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while !path.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "{path:?} never appeared"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[test]
fn o_cli_child() {
    let Some(args) = std::env::var_os(CHILD) else {
        return;
    };
    let args: Vec<String> = serde_json::from_str(args.to_str().unwrap()).unwrap();
    let cli = TestCli::try_parse_from(std::iter::once("agentdesk".into()).chain(args)).unwrap();
    crate::cli::execute(cli.command, false).unwrap();
}

/// A store whose channel 7 holds one piece refused with 403 at serial 0.
fn rejected() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    let config = StoreConfig { enabled: true };
    let store = OStore::open_if_enabled(&config, root.path())
        .unwrap()
        .unwrap();
    let init = |channel| {
        let (sources, initial_anchor, build_digest, at) = (Vec::new(), 100, "b".into(), Utc::now());
        Ok(Initialized {
            channel,
            sources,
            initial_anchor,
            build_digest,
            at,
        })
    };
    let era = store.begin_era(&[CHANNEL], Utc::now(), init).unwrap();
    let mut channel = store.open_channel(&era, CHANNEL).unwrap().unwrap();
    let unit_key = UnitKey {
        channel_id: CHANNEL,
        provider: ShadowProvider::Claude,
        native_key: "original".into(),
        kind: UnitKind::Body,
    };
    let (payload, anchor_id, epoch) = ("original bytes".into(), 100, 1);
    let prepared = LedgerEntry::Prepared {
        serial: 0,
        unit_key,
        piece_index: 0,
        payload,
        anchor_id,
        epoch,
    };
    channel.append_ledger(prepared).unwrap();
    let refused = LedgerEntry::Rejected {
        serial: 0,
        status: 403,
    };
    channel.append_ledger(refused).unwrap();
    root
}

fn files(path: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    for entry in std::fs::read_dir(path).unwrap() {
        let path = entry.unwrap().path();
        match path.is_dir() {
            true => files.extend(self::files(&path)),
            false => drop(files.insert(path.clone(), std::fs::read(path).unwrap())),
        }
    }
    files
}

fn ledger(root: &Path) -> PathBuf {
    root.join("o_store/7/ledger.jsonl")
}

fn approvals(root: &Path) -> usize {
    let ledger = std::fs::read_to_string(ledger(root)).unwrap();
    ledger.matches("\"operator_resume\"").count()
}

fn floor(root: &Path) -> bool {
    root.join("o_store/operator_resume.floor").exists()
}

fn resume(root: &Path, reason: &str) -> (Option<i32>, String, String) {
    let args = ["o", "resume", "--channel", "7", "--rejected-serial", "0"];
    run_cli(root, &[&args[..], &["--reason", reason]].concat())
}

fn status(root: &Path) -> String {
    let before = files(root);
    let (code, stdout, stderr) = run_cli(root, &["o", "status", "--channel", "7"]);
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!(files(root), before, "status wrote under the runtime root");
    stdout
}

#[test]
fn o_status_reports_the_refused_piece_and_writes_nothing() {
    let root = rejected();
    let shown = status(root.path());
    assert!(
        shown.contains("channel 7: blocked serial 0 (403)"),
        "{shown}"
    );
    assert!(shown.contains("serial 0 rejected 403: Claude original Body piece 0"));
    assert!(shown.contains("latest 0 Rejected(403); Blocked { serial: 0, status: 403 }"));
    assert!(shown.contains("approval absent"), "{shown}");
    assert!(!floor(root.path()) && approvals(root.path()) == 0);
}

#[test]
fn o_resume_records_one_approval_and_a_repeat_reports_it_without_writing() {
    let root = rejected();
    let (code, stdout, stderr) = resume(root.path(), "permissions restored");
    assert_eq!(code, Some(0), "{stderr}");
    assert!(stdout.contains("for serial 0 is durable; nothing was sent yet"));
    assert!(floor(root.path()) && approvals(root.path()) == 1);
    let shown = status(root.path());
    assert!(
        shown.contains("Authorized { rejected_serial: 0 }"),
        "{shown}"
    );
    assert!(shown.contains("approval unconsumed, "), "{shown}");
    assert!(shown.contains("by \"operator\""), "{shown}");
    assert!(shown.contains("\"permissions restored\""), "{shown}");
    let bytes = std::fs::read(ledger(root.path())).unwrap();
    let (code, _, stderr) = resume(root.path(), "another reason");
    assert_eq!(code, Some(3), "{stderr}");
    assert!(stderr.contains("serial 0 already has approval"), "{stderr}");
    assert!(
        stderr.contains("\"permissions restored\" (unconsumed)"),
        "{stderr}"
    );
    assert_eq!(std::fs::read(ledger(root.path())).unwrap(), bytes);
}

#[test]
fn o_resume_refusals_exit_with_their_own_code_and_write_nothing() {
    for (case, expected, words) in [
        ("serial", 4, "refused, nothing was written"),
        ("blank", 4, "refused, nothing was written"),
        ("busy", 5, "the ledger is locked"),
        ("floor", 6, "the rollback floor is absent"),
        ("tail", 4, "unfinished ledger entry"),
        ("unknown", 7, "the approval may be durable"),
        ("damage", 6, "StoreDamage"),
        ("damage_floor", 7, "StoreDamage"),
    ] {
        let root = rejected();
        let marker = root.path().join("o_store/operator_resume.floor");
        match case {
            "tail" => {
                let options = std::fs::OpenOptions::new().append(true).clone();
                let mut file = options.open(ledger(root.path())).unwrap();
                file.write_all(b"{").unwrap();
            }
            "unknown" => std::fs::write(&marker, b"1\n").unwrap(),
            "damage" | "damage_floor" => {
                let options = std::fs::OpenOptions::new().append(true).clone();
                let mut file = options.open(ledger(root.path())).unwrap();
                file.write_all(b"{}\n").unwrap();
                if case == "damage_floor" {
                    std::fs::write(&marker, b"1\n").unwrap();
                }
            }
            _ => {}
        }
        let before = files(root.path());
        let args = ["o", "resume", "--channel", "7", "--reason"];
        let held = std::fs::OpenOptions::new()
            .read(true)
            .append(true)
            .open(ledger(root.path()))
            .unwrap();
        let store_dir = root.path().join("o_store");
        let (code, _, stderr) = match case {
            "serial" => run_cli(
                root.path(),
                &[&args[..], &["r", "--rejected-serial", "9"]].concat(),
            ),
            "blank" => run_cli(
                root.path(),
                &[&args[..], &[" ", "--rejected-serial", "0"]].concat(),
            ),
            "busy" => {
                held.try_lock().unwrap();
                resume(root.path(), "restored")
            }
            "tail" | "damage" | "damage_floor" => resume(root.path(), "restored"),
            "unknown" => {
                let other = ["o", "resume", "--channel", "8", "--rejected-serial", "0"];
                run_cli(root.path(), &[&other[..], &["--reason", "r"]].concat())
            }
            _ => {
                let mode = |mode| std::fs::Permissions::from_mode(mode);
                std::fs::set_permissions(&store_dir, mode(0o555)).unwrap();
                let result = resume(root.path(), "restored");
                std::fs::set_permissions(&store_dir, mode(0o755)).unwrap();
                result
            }
        };
        drop(held);
        assert_eq!(code, Some(expected), "{case}: {stderr}");
        assert!(stderr.contains(words), "{case}: {stderr}");
        if case.starts_with("damage") {
            assert!(stderr.contains("ledger byte"), "{case}: {stderr}");
        }
        assert_eq!(files(root.path()), before, "{case}");
        let floored = matches!(case, "unknown" | "damage_floor");
        assert_eq!(floor(root.path()), floored, "{case}");
    }
}

const SAME: [&str; 8] = [
    "o",
    "resume",
    "--channel",
    "7",
    "--rejected-serial",
    "0",
    "--reason",
    "restored",
];

#[test]
fn o_resume_two_racing_identical_commands_leave_one_approval_and_one_exit_zero() {
    let root = rejected();
    let racers = [spawn(root.path(), &SAME), spawn(root.path(), &SAME)];
    let codes = racers.map(|racer| racer.wait_with_output().unwrap().status.code());
    assert!(
        matches!(codes, [Some(0), Some(3 | 5)] | [Some(3 | 5), Some(0)]),
        "racing approvals exited {codes:?}"
    );
    assert_eq!(approvals(root.path()), 1);
}

// The loser of two identical commands starts before the winner writes and reaches the store
// after it: it exits 3 with the winner's approval and writes nothing.
#[test]
fn o_resume_identical_command_overtaken_by_another_reports_the_existing_approval() {
    let root = rejected();
    let gate = tempfile::tempdir().unwrap();
    let mut loser = command(root.path(), &SAME);
    let loser = loser.env(PAUSE, gate.path()).spawn().unwrap();
    wait_for(&gate.path().join("paused"));
    let (code, stdout, stderr) = run_cli(root.path(), &SAME);
    assert_eq!(code, Some(0), "{stderr}");
    let id = stdout
        .split("approval ")
        .nth(1)
        .unwrap()
        .split(' ')
        .next()
        .unwrap();
    let written = std::fs::read(ledger(root.path())).unwrap();
    std::fs::write(gate.path().join("go"), b"").unwrap();
    let output = loser.wait_with_output().unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(output.status.code(), Some(3), "{stderr}");
    let existing = format!("serial 0 already has approval {id} by \"operator\"");
    assert!(stderr.contains(&existing), "{stderr}");
    assert_eq!(std::fs::read(ledger(root.path())).unwrap(), written);
    assert_eq!(approvals(root.path()), 1);
}

#[test]
fn o_status_failures_exit_non_zero_without_a_header_or_a_write() {
    for (case, words) in [
        ("tail", "unfinished ledger entry"),
        ("busy", "WouldBlock"),
        ("no_channel", "NotFound"),
        ("no_store", "no O store under the runtime root"),
    ] {
        let root = match case {
            "no_store" => tempfile::tempdir().unwrap(),
            _ => rejected(),
        };
        if case == "tail" {
            let options = std::fs::OpenOptions::new().append(true).clone();
            let mut file = options.open(ledger(root.path())).unwrap();
            file.write_all(b"{").unwrap();
        }
        let held = (case == "busy").then(|| {
            let options = std::fs::OpenOptions::new().read(true).append(true).clone();
            let file = options.open(ledger(root.path())).unwrap();
            file.try_lock().unwrap();
            file
        });
        let before = files(root.path());
        let channel = if case == "no_channel" { "8" } else { "7" };
        let (code, stdout, stderr) = run_cli(root.path(), &["o", "status", "--channel", channel]);
        drop(held);
        assert_eq!(code, Some(1), "{case}: {stderr}");
        assert!(stderr.contains(words), "{case}: {stderr}");
        assert!(!stdout.contains("blocked"), "{case}: {stdout}");
        assert_eq!(files(root.path()), before, "{case}");
        assert!(!floor(root.path()), "{case}");
    }
}
