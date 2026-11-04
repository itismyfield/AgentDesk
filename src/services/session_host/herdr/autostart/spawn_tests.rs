//! The ready wait against a socket this test serves: a server counts once its hello answers.
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::sync::atomic::{AtomicBool, Ordering};

use super::*;

/// Accepts every dial; answers a ping only once `answering` is set, else holds the connection.
fn serve(socket: &Path, answering: Arc<AtomicBool>) {
    let listener = UnixListener::bind(socket).unwrap();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for stream in listener.incoming().flatten() {
            if !answering.load(Ordering::SeqCst) {
                held.push(stream);
                continue;
            }
            let mut line = String::new();
            let _ = BufReader::new(&stream).read_line(&mut line);
            let id = serde_json::from_str::<Value>(&line).unwrap_or_default()["id"].clone();
            let pong =
                json!({"id": id, "result": {"type": "pong", "version": "0.9.5", "protocol": 22}});
            let _ = (&stream).write_all(format!("{pong}\n").as_bytes());
        }
    });
}

#[tokio::test(flavor = "current_thread")]
async fn a_started_server_is_one_whose_hello_answered() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("herdr.sock");
    let answering = Arc::new(AtomicBool::new(false));
    serve(&socket, answering.clone());
    let child = || Child {
        pid: std::process::id(),
        exited: Arc::new(Mutex::new(None)),
    };
    let mut value = json!({});
    let wait = Duration::from_millis(600);
    let silent = ready(&socket, child(), wait, &mut value).await;
    assert_eq!(
        silent,
        Ensured::Refused("not_ready"),
        "a socket that only accepts"
    );
    answering.store(true, Ordering::SeqCst);
    let pid = std::process::id();
    let answered = ready(&socket, child(), Duration::from_secs(5), &mut value).await;
    assert_eq!(answered, Ensured::Started(pid));
}
