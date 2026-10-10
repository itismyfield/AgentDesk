#![cfg(unix)]

use super::tests::{enabled, sealed};
use super::*;
use crate::services::tui_o::shadow::{ShadowProvider, UnitKey, UnitKind};
use ledger::{PieceDisposition, PieceOutcome};
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};

fn key() -> UnitKey {
    UnitKey {
        channel_id: 7,
        provider: ShadowProvider::Claude,
        native_key: "original".into(),
        kind: UnitKind::Body,
    }
}

fn prepared(serial: u64) -> LedgerEntry {
    LedgerEntry::Prepared {
        serial,
        unit_key: key(),
        piece_index: 0,
        payload: "original bytes".into(),
        anchor_id: 100,
        epoch: 1,
    }
}

fn rejected() -> (tempfile::TempDir, OStore, PathBuf) {
    let runtime = tempfile::tempdir().unwrap();
    let store = enabled(runtime.path());
    let era = sealed(&store, &[7]);
    let mut channel = store.open_channel(&era, 7).unwrap().unwrap();
    channel.append_ledger(prepared(0)).unwrap();
    channel
        .append_ledger(LedgerEntry::Rejected {
            serial: 0,
            status: 403,
        })
        .unwrap();
    let path = store.channel_dir(7).join(LEDGER_FILE);
    (runtime, store, path)
}

fn approve(store: &OStore) -> ledger::ResumeApproval {
    store
        .record_operator_resume(7, 0, "operator", "permissions restored")
        .unwrap()
}

fn files_under(path: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    for entry in std::fs::read_dir(path).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files.extend(files_under(&path));
        } else {
            files.insert(path.clone(), std::fs::read(path).unwrap());
        }
    }
    files
}

#[test]
fn operator_resume_floor_precedes_append_and_duplicate_is_read_only_after_consumption() {
    let (runtime, store, path) = rejected();
    let before = store.operator_resume_status(7).unwrap();
    let marker = runtime.path().join("o_store/operator_resume.floor");
    let approval = store
        .record_operator_resume_with_append(7, 0, "first", "restored", |file, at, entry| {
            assert_eq!(std::fs::read(&marker).unwrap(), b"1\n");
            ledger::append_to(file, at, entry)
        })
        .unwrap();
    assert!(!approval.approval_id.is_nil());
    let status = store.operator_resume_status(7).unwrap();
    assert_eq!(
        (status.next_serial(), status.anchor()),
        (before.next_serial(), before.anchor())
    );
    assert_eq!(status.piece(0), before.piece(0));
    assert_eq!(status.unresolved(), None);
    assert_eq!(
        status.disposition(&key(), 0),
        PieceDisposition::Authorized { rejected_serial: 0 }
    );
    let era = store.read_era().unwrap().unwrap();
    let mut channel = store.open_channel(&era, 7).unwrap().unwrap();
    channel.append_ledger(prepared(1)).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let repeated = store
        .record_operator_resume(7, 0, "replacement", "new reason")
        .unwrap();
    assert_eq!(repeated.approval_id, approval.approval_id);
    assert_eq!(
        (
            &*repeated.operator,
            &*repeated.reason,
            repeated.consumed_serial
        ),
        ("first", "restored", Some(1))
    );
    assert_eq!(std::fs::read(path).unwrap(), bytes);
}

#[test]
fn operator_resume_validation_refuses_invalid_stale_open_and_damaged_without_writes() {
    for case in [
        "serial",
        "status",
        "stale",
        "open",
        "posted",
        "tail",
        "bad_json",
        "violation",
        "blank_operator",
        "blank_reason",
        "missing_init",
        "missing_era",
    ] {
        let (runtime, store, path) = rejected();
        let era = store.read_era().unwrap().unwrap();
        let mut channel = store.open_channel(&era, 7).unwrap().unwrap();
        match case {
            "status" => channel
                .append_ledger(LedgerEntry::Rejected {
                    serial: 0,
                    status: 429,
                })
                .unwrap(),
            "stale" | "open" | "posted" => {
                channel.append_ledger(prepared(1)).unwrap();
                if case == "stale" {
                    channel
                        .append_ledger(LedgerEntry::Rejected {
                            serial: 1,
                            status: 404,
                        })
                        .unwrap();
                }
                if case == "posted" {
                    channel
                        .append_ledger(LedgerEntry::Posted {
                            serial: 1,
                            msg_id: 200,
                        })
                        .unwrap();
                }
            }
            "violation" => channel.append_ledger(prepared(2)).unwrap(),
            "tail" | "bad_json" => {
                let mut file = OpenOptions::new().append(true).open(&path).unwrap();
                file.write_all(if case == "tail" { b"{" } else { b"{}\n" })
                    .unwrap();
            }
            "missing_init" => std::fs::remove_file(store.channel_dir(7).join(INIT_FILE)).unwrap(),
            "missing_era" => std::fs::remove_file(store.root.join(ERA_FILE)).unwrap(),
            _ => {}
        }
        drop(channel);
        let before = std::fs::read(&path).unwrap();
        let all_files = files_under(runtime.path());
        let serial = if case == "serial" { 9 } else { 0 };
        let operator = if case == "blank_operator" {
            " \n"
        } else {
            "operator"
        };
        let reason = if case == "blank_reason" {
            "\t"
        } else {
            "restored"
        };
        assert!(
            store
                .record_operator_resume(7, serial, operator, reason)
                .is_err(),
            "{case}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), before, "{case}");
        assert_eq!(files_under(runtime.path()), all_files, "{case}");
        assert!(
            !runtime
                .path()
                .join("o_store/operator_resume.floor")
                .exists(),
            "{case}"
        );
    }
}

#[test]
fn operator_resume_writer_append_is_refused_without_a_disk_change() {
    let (_runtime, store, path) = rejected();
    let era = store.read_era().unwrap().unwrap();
    let mut channel = store.open_channel(&era, 7).unwrap().unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let entry = LedgerEntry::OperatorResume {
        rejected_serial: 0,
        approval_id: uuid::Uuid::new_v4(),
        operator: "writer".into(),
        reason: "bypass".into(),
        at: Utc::now(),
    };
    assert!(matches!(
        channel.append_ledger(entry),
        Err(StoreError::Rejected(_))
    ));
    assert_eq!(std::fs::read(path).unwrap(), bytes);
    assert!(channel.ledger().approval(0).is_none());
}

#[test]
fn operator_resume_append_failure_does_not_claim_absence_or_repair_a_partial_tail() {
    for fraction in [0, 1, 2] {
        let (runtime, store, path) = rejected();
        let result = store.record_operator_resume_with_append(
            7,
            0,
            "original operator",
            "original reason",
            |file, at, entry| {
                let mut bytes =
                    serde_json::to_vec(&serde_json::json!({"at":at,"entry":entry})).unwrap();
                bytes.push(b'\n');
                file.write_all(&bytes[..bytes.len() * fraction / 2])?;
                Err(io::Error::from(io::ErrorKind::StorageFull).into())
            },
        );
        assert!(matches!(result, Err(StoreError::Io(_))));
        assert!(
            runtime
                .path()
                .join("o_store/operator_resume.floor")
                .exists()
        );
        let before = std::fs::read(&path).unwrap();
        if fraction == 1 {
            assert!(
                store
                    .record_operator_resume(7, 0, "retry", "retry")
                    .is_err()
            );
            assert_eq!(std::fs::read(&path).unwrap(), before);
        } else {
            let approval = store
                .record_operator_resume(7, 0, "retry", "retry")
                .unwrap();
            assert_eq!(approval.consumed_serial, None);
            if fraction == 2 {
                assert_eq!(approval.operator, "original operator");
                assert_eq!(std::fs::read(&path).unwrap(), before);
            }
        }
    }
}

#[test]
fn operator_resume_floor_write_failure_leaves_the_approval_absent() {
    let (runtime, store, path) = rejected();
    let before = std::fs::read(&path).unwrap();
    let _fault = fault::plant(
        runtime.path(),
        fault::Step::Write,
        io::ErrorKind::StorageFull,
        Some(1),
    );
    assert!(approve_result(&store).is_err());
    assert_eq!(std::fs::read(path).unwrap(), before);
    assert!(
        !runtime
            .path()
            .join("o_store/operator_resume.floor")
            .exists()
    );
}

fn approve_result(store: &OStore) -> Result<ledger::ResumeApproval, StoreError> {
    store.record_operator_resume(7, 0, "operator", "restored")
}

fn marker(reader: &mut impl BufRead, expected: &str) {
    let mut line = String::new();
    loop {
        line.clear();
        assert_ne!(
            reader.read_line(&mut line).unwrap(),
            0,
            "missing {expected}"
        );
        if line.trim() == expected {
            return;
        }
    }
}

#[test]
fn operator_resume_two_processes_share_validation_append_lock_and_first_approval() {
    let (runtime, store, path) = rejected();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "services::tui_o::store::operator_resume_tests::operator_resume_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("AGENTDESK_OPERATOR_RESUME_CHILD", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let approval = store
        .record_operator_resume_with_append(7, 0, "parent", "parent reason", |file, at, entry| {
            writeln!(input, "{}", runtime.path().display()).unwrap();
            input.flush().unwrap();
            marker(&mut output, "BUSY");
            ledger::append_to(file, at, entry)
        })
        .unwrap();
    writeln!(input, "retry").unwrap();
    input.flush().unwrap();
    marker(&mut output, "EXISTING");
    drop(input);
    let mut rest = String::new();
    output.read_to_string(&mut rest).unwrap();
    assert!(child.wait().unwrap().success(), "{rest}");
    assert_eq!(
        store.operator_resume_status(7).unwrap().approval(0),
        Some(&approval)
    );
    assert_eq!(
        std::fs::read_to_string(path)
            .unwrap()
            .matches("operator_resume")
            .count(),
        1
    );
}

#[test]
fn operator_resume_child() {
    if std::env::var_os("AGENTDESK_OPERATOR_RESUME_CHILD").is_none() {
        return;
    }
    let mut input = BufReader::new(std::io::stdin());
    let mut line = String::new();
    input.read_line(&mut line).unwrap();
    let store = OStore::existing(Path::new(line.trim())).unwrap();
    assert!(
        matches!(approve_result(&store), Err(StoreError::Io(ref e)) if e.kind() == io::ErrorKind::WouldBlock)
    );
    println!("\nBUSY");
    std::io::stdout().flush().unwrap();
    line.clear();
    input.read_line(&mut line).unwrap();
    assert_eq!(approve(&store).operator, "parent");
    println!("\nEXISTING");
    std::io::stdout().flush().unwrap();
}

#[test]
fn operator_resume_ledger_lock_can_hold_initial_recovery_then_next_restart_recovers() {
    let (_runtime, store, path) = rejected();
    let era = store.read_era().unwrap().unwrap();
    let file = durable::LockedFile::try_lock(
        OpenOptions::new()
            .read(true)
            .append(true)
            .open(path)
            .unwrap(),
    )
    .unwrap();
    let error = store.open_channel(&era, 7).err().unwrap();
    assert_eq!(error.io, Some(io::ErrorKind::WouldBlock));
    drop(file);
    approve(&store);
    let reopened = store.open_channel(&era, 7).unwrap().unwrap();
    assert_eq!(
        reopened.ledger().piece(0).unwrap().outcome,
        Some(PieceOutcome::Rejected(403))
    );
    assert!(matches!(
        reopened.ledger().disposition(&key(), 0),
        PieceDisposition::Authorized { .. }
    ));
}
