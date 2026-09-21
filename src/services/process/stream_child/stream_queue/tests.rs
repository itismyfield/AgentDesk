use super::*;
use std::sync::Barrier;

#[test]
fn seal_preserves_published_terminal_and_rejects_every_sender_clone() {
    let (tx, rx) = channel();
    let clone = tx.clone();
    tx.send("progress").unwrap();
    clone.send("terminal").unwrap();
    rx.seal();
    assert!(tx.send("late").is_err());
    assert!(clone.send("later").is_err());
    assert_eq!(rx.recv_timeout(Duration::ZERO).unwrap(), "progress");
    assert_eq!(rx.recv_timeout(Duration::ZERO).unwrap(), "terminal");
    assert_eq!(
        rx.recv_timeout(Duration::ZERO),
        Err(mpsc::RecvTimeoutError::Disconnected)
    );
}

#[test]
fn concurrent_seal_includes_a_racing_send_if_and_only_if_publication_succeeds() {
    for _ in 0..64 {
        let (tx, rx) = channel();
        let racing = tx.clone();
        let start = Arc::new(Barrier::new(2));
        let producer_start = start.clone();
        let producer = std::thread::spawn(move || {
            producer_start.wait();
            racing.send("racing").is_ok()
        });
        start.wait();
        rx.seal();
        let published = producer.join().unwrap();
        assert!(tx.send("after seal").is_err());
        assert_eq!(
            rx.recv_timeout(Duration::ZERO).ok(),
            published.then_some("racing")
        );
        assert_eq!(
            rx.recv_timeout(Duration::ZERO),
            Err(mpsc::RecvTimeoutError::Disconnected)
        );
    }
}
