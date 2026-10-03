#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;

use super::*;
use crate::services::tui_o::shadow::binding_reader::source_id_for;
use crate::services::tui_o::store::rotation::{Boundary, ResolveFrom, Rotation, SourceLink};
use crate::services::tui_o::store::spool::source_key;
use crate::services::tui_o::store::tests::{enabled, initialized};
use crate::services::tui_o::store::{LEDGER_FILE, OStore, STORE_DIR_NAME};

fn pending(runtime: &Path) -> (OStore, SourceId, std::path::PathBuf) {
    let source_path = runtime.join("source.jsonl");
    std::fs::write(&source_path, b"{}\n").unwrap();
    let source = source_id_for("lock-test", &source_path).unwrap();
    let store = enabled(runtime);
    let era = store
        .begin_era(&[7], Utc::now(), |channel| {
            Ok(initialized(channel, Vec::new()))
        })
        .unwrap();
    let mut channel = store.open_channel(&era, 7).unwrap().unwrap();
    let mut rotation = Rotation::default();
    rotation.links.insert(
        source_key(&source),
        SourceLink {
            source: source.clone(),
            seq: 1,
            parent: None,
            committed_at: Utc::now(),
            boundary: Boundary::Pending {
                candidates: vec![0],
            },
        },
    );
    channel.write_rotation(&rotation).unwrap();
    let ledger = runtime.join(STORE_DIR_NAME).join("7").join(LEDGER_FILE);
    (store, source, ledger)
}

#[test]
fn successful_operator_append_survives_recovery_tail_truncation() {
    let runtime = tempfile::tempdir().unwrap();
    let (store, source, path) = pending(runtime.path());
    let key = source_key(&source);
    let (partial_tx, partial_rx) = mpsc::channel();
    let (finish_tx, finish_rx) = mpsc::channel();
    let (success_tx, success_rx) = mpsc::channel();
    let operator = std::thread::spawn(move || {
        let result = store.record_boundary_resolved_with_append(
            7,
            &key,
            &ResolveFrom::Offset(0),
            "operator",
            |file, at, entry| {
                let mut bytes = serde_json::to_vec(&LedgerLine {
                    at,
                    entry: entry.clone(),
                })?;
                bytes.push(b'\n');
                file.write_all(&bytes[..1])?;
                partial_tx.send(()).unwrap();
                finish_rx.recv().unwrap();
                file.write_all(&bytes[1..])?;
                Ok(file.sync_data()?)
            },
        );
        success_tx.send(result.is_ok()).unwrap();
        result
    });
    partial_rx.recv().unwrap();
    let recovery = recover_with_tail(&path, 100, || {
        finish_tx.send(()).unwrap();
        assert!(
            success_rx.recv().unwrap(),
            "operator must report success before truncate"
        );
    });
    if let Err(StoreError::Io(error)) = &recovery {
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        finish_tx.send(()).unwrap();
    } else {
        recovery.unwrap();
    }
    assert_eq!(operator.join().unwrap().unwrap(), (source.clone(), 0));
    assert_eq!(
        recover(&path, 100).unwrap().boundary_resolved(&source),
        Some(0),
        "operator success must survive restart"
    );
}

fn read_marker(reader: &mut impl BufRead, marker: &str) {
    let mut line = String::new();
    loop {
        line.clear();
        assert_ne!(reader.read_line(&mut line).unwrap(), 0, "missing {marker}");
        if line.trim() == marker {
            return;
        }
    }
}

#[test]
fn recovery_excludes_an_operator_in_a_separate_process() {
    let runtime = tempfile::tempdir().unwrap();
    let (_store, source, path) = pending(runtime.path());
    std::fs::write(&path, b"{").unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "services::tui_o::store::ledger::lock_tests::operator_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("AGENTDESK_LEDGER_LOCK_CHILD", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    recover_with_tail(&path, 100, || {
        writeln!(input, "{}", runtime.path().display()).unwrap();
        input.flush().unwrap();
        read_marker(&mut output, "LOCK_REJECTED");
    })
    .unwrap();
    writeln!(input, "retry").unwrap();
    input.flush().unwrap();
    read_marker(&mut output, "OPERATOR_SUCCESS");
    drop(input);
    let mut rest = String::new();
    output.read_to_string(&mut rest).unwrap();
    assert!(child.wait().unwrap().success(), "{rest}");
    assert_eq!(
        recover(&path, 100).unwrap().boundary_resolved(&source),
        Some(0)
    );
}

#[test]
fn operator_child() {
    if std::env::var_os("AGENTDESK_LEDGER_LOCK_CHILD").is_none() {
        return;
    }
    let mut input = BufReader::new(std::io::stdin());
    let mut runtime = String::new();
    if input.read_line(&mut runtime).unwrap() == 0 {
        return;
    }
    let runtime = Path::new(runtime.trim());
    let store = OStore::existing(runtime).unwrap();
    let source = runtime.join("source.jsonl");
    let resolve = || {
        store.record_boundary_resolved(
            7,
            source.to_str().unwrap(),
            &ResolveFrom::Offset(0),
            "child-operator",
        )
    };
    let error = resolve().unwrap_err();
    assert!(matches!(error, StoreError::Io(ref e) if e.kind() == io::ErrorKind::WouldBlock));
    println!("\nLOCK_REJECTED");
    std::io::stdout().flush().unwrap();
    let mut retry = String::new();
    assert_ne!(input.read_line(&mut retry).unwrap(), 0);
    resolve().unwrap();
    println!("OPERATOR_SUCCESS");
    std::io::stdout().flush().unwrap();
}
