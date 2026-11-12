//! Finish may race either side of a busy write; its receipt still owns the original.
use super::*;
use std::io::{Read, Write};
use std::os::unix::net::UnixListener;

fn quoted(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}
fn finish_race(phase: &str, channel: u64) {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = rig(
        &format!("busy-finish-{phase}"),
        channel,
        "normal",
        view("", false),
        BUSY,
    );
    let _trace = rig.trace_driver();
    running_row(&rig, &rig.name);
    let path = rig.transcript();
    rig.mark(
        phase,
        &format!(
            "printf '%s\\n' '{{\"type\":\"system\",\"subtype\":\"turn_duration\"}}' >> {}\n",
            quoted(&path)
        ),
    );
    let socket = rig.f.root.path().join("receipt-barrier.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    rig.mark("after-Enter",&format!(
        "python3 -c \"import socket; s=socket.socket(socket.AF_UNIX); s.connect({}); s.sendall(b'written'); assert s.recv(1)==b'1'\"\n",
        quoted(&socket)));
    let (drive, inbox) = rig.drive();
    let duplicate_drive = drive.clone();
    let mailbox = rig.mailbox.clone();
    let handle = rig.runtime.handle().clone();
    let observer = std::thread::spawn(move || {
        let (mut connection, _) = listener.accept().unwrap();
        connection
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut message = [0; 7];
        connection.read_exact(&mut message).unwrap();
        assert_eq!(&message, b"written");
        let _entered = handle.enter();
        assert!(
            handle
                .block_on(mailbox.v2_request(|slot| Ok(slot.as_ref().unwrap().front().is_none())))
                .unwrap(),
            "the original is unavailable before duplicate wakes can claim it"
        );
        crate::services::discord::queue_io::input_owner::drive::wake(drive.clone());
        crate::services::discord::queue_io::input_owner::drive::wake(drive);
        handle.block_on(async {
            tokio::task::yield_now().await;
            assert!(
                mailbox
                    .v2_request(|slot| Ok(slot.as_ref().unwrap().front().is_none()))
                    .await
                    .unwrap(),
                "written busy original must stay unavailable to duplicate Idle wake"
            );
        });
        assert!(matches!(
            inbox.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        connection.write_all(b"1").unwrap();
        inbox
    });
    let result = rig.runtime.block_on(hand_busy(
        (rig.mailbox.clone(), None),
        ProviderKind::Claude,
        (channel, rig.granted_front()),
        None,
    ));
    assert_eq!(result, Ok(()));
    let inbox = crate::test_support::wait::join_within(observer, Duration::from_secs(10));
    rig.assert_hooks_completed();
    assert_eq!(rig.events(), ["paste", "Enter"]);
    let rows = fs::read_to_string(&path).unwrap();
    assert_eq!(
        rows.lines()
            .filter(|line| {
                let row = serde_json::from_str::<Value>(line).unwrap();
                row["type"] == "user"
                    && row["message"]["content"]
                        .as_str()
                        .is_some_and(|text| text.contains(PROMPT))
            })
            .count(),
        1,
        "the renderer executed exactly one original"
    );
    let snapshot = rig.snapshot();
    assert!(snapshot["head_injection"].is_null());
    assert_eq!(snapshot["items"], json!([]));
    assert_eq!(snapshot["retained_inputs"], json!([]));
    assert_eq!(snapshot["terminal_identities"].as_array().unwrap().len(), 1);
    assert!(matches!(rig.bind(),TurnBind::Settled(turn) if !turn.attempt.is_empty()));
    rig.wake(&duplicate_drive);
    assert!(matches!(
        inbox.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
    ));
}
#[test]
fn finish_after_busy_selection_keeps_one_write_execution_and_settlement() {
    finish_race("before-paste", 7_755_080);
}
#[test]
fn finish_after_busy_write_keeps_one_write_execution_and_settlement() {
    finish_race("after-paste", 7_755_081);
}
