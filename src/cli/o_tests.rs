//! `agentdesk o status|resume` in a child process through the real subcommand parser and
//! dispatcher: status writes nothing, and only a fresh durable approval exits 0.
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
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "cli::o::tests::o_cli_child", "--nocapture"])
        .args(["--test-threads=1"])
        .env("AGENTDESK_ROOT_DIR", root)
        .env(CHILD, serde_json::to_string(args).unwrap())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
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
            "tail" => resume(root.path(), "restored"),
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
        assert_eq!(files(root.path()), before, "{case}");
        assert_eq!(floor(root.path()), case == "unknown", "{case}");
    }
}

#[test]
fn o_resume_two_racing_operators_leave_one_approval_and_one_exit_zero() {
    let root = rejected();
    let args = |reason| {
        [
            "o",
            "resume",
            "--channel",
            "7",
            "--rejected-serial",
            "0",
            "--reason",
            reason,
        ]
    };
    let racers = [
        spawn(root.path(), &args("first")),
        spawn(root.path(), &args("second")),
    ];
    let codes = racers.map(|racer| racer.wait_with_output().unwrap().status.code());
    let winner = match codes {
        [Some(0), Some(3 | 5)] => "first",
        [Some(3 | 5), Some(0)] => "second",
        other => panic!("racing approvals exited {other:?}"),
    };
    assert_eq!(approvals(root.path()), 1);
    let shown = status(root.path());
    assert!(shown.contains(&format!("\"{winner}\"")), "{shown}");
}
