//! A queue head the receiver keeps answering 425 is resent by the real worker on a growing delay,
//! while a caller waiting on the same queue still gets its reply inside its window.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use super::*;

/// `(ordinal, attempt from 1, time since the ordinal's first send)` decides the status.
type Rule = Arc<dyn Fn(u64, usize, Duration) -> u16 + Send + Sync>;

/// `(ordinal, arrival)` of every request the receiver read.
type Sends = Arc<Mutex<Vec<(u64, Instant)>>>;

/// A receiver answering each request as `rule` says, and the real worker loop on one queue.
struct Relay {
    endpoint: String,
    sends: Sends,
    stop: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
    _root: (tempfile::TempDir, crate::config::TestEnvVarGuard),
}

impl Relay {
    fn new(rule: Rule) -> Self {
        let root = tempfile::tempdir().unwrap();
        let guard = crate::config::set_agentdesk_root_for_test(root.path());
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let (sends, stop) = (Sends::default(), Arc::new(AtomicBool::new(false)));
        let receiver = {
            let (sends, stop) = (sends.clone(), stop.clone());
            std::thread::spawn(move || serve(listener, rule, sends, stop))
        };
        Self {
            endpoint,
            sends,
            stop,
            threads: vec![receiver],
            _root: (root, guard),
        }
    }

    /// Queues a hook nobody waits on and returns its queue.
    fn enqueue(&self, session: &str, ordinal: u64) -> PathBuf {
        self.enqueue_payload(session, json!({ "ordinal": ordinal }))
    }

    fn enqueue_payload(&self, session: &str, payload: Value) -> PathBuf {
        let enqueued = enqueue_ordered_hook_relay_request(
            &self.endpoint,
            "claude",
            "PostToolUse",
            session,
            payload,
            None,
        );
        enqueued.unwrap().0
    }

    /// Runs the production worker on `queue` until the relay drops.
    fn work(&mut self, queue: PathBuf) {
        let stop = self.stop.clone();
        self.threads.push(std::thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                let request = OrderedHookRelayWorkerRequest {
                    queue_dir: queue.clone(),
                };
                let _ = run_ordered_hook_relay_worker(request);
                std::thread::sleep(Duration::from_millis(2));
            }
        }));
    }

    /// Arrival times of `ordinal`'s sends, once `count` of them arrived.
    fn sends_of(&self, ordinal: u64, count: usize) -> Vec<Instant> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let sends = self.sends.lock().unwrap().clone();
            let of: Vec<_> = sends
                .iter()
                .filter(|(o, _)| *o == ordinal)
                .map(|(_, at)| *at)
                .collect();
            if of.len() >= count || Instant::now() > deadline {
                assert!(
                    of.len() >= count,
                    "{ordinal}: {} of {count} sends",
                    of.len()
                );
                return of;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        for thread in self.threads.drain(..) {
            let deadline = Instant::now() + Duration::from_secs(3);
            while !thread.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
}

fn serve(listener: std::net::TcpListener, rule: Rule, sends: Sends, stop: Arc<AtomicBool>) {
    let mut first: std::collections::HashMap<u64, (Instant, usize)> = Default::default();
    while !stop.load(Ordering::Acquire) {
        let mut socket = match listener.accept() {
            Ok((socket, _)) => socket,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(1));
                continue;
            }
            Err(error) => panic!("accept: {error}"),
        };
        socket.set_nonblocking(false).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut encoded = Vec::new();
        let mut buffer = [0u8; 4096];
        let complete = loop {
            let Ok(read) = socket.read(&mut buffer) else {
                break false;
            };
            encoded.extend_from_slice(&buffer[..read]);
            match http_body_bounds(&encoded) {
                Some((start, len)) if encoded.len() >= start + len => break true,
                _ if read == 0 => break false,
                _ => {}
            }
        };
        if !complete {
            continue;
        }
        let arrived = Instant::now();
        let ordinal = request_body(&encoded)["ordinal"].as_u64().unwrap();
        let (since, attempt) = first.entry(ordinal).or_insert((arrived, 0));
        *attempt += 1;
        let status = rule(ordinal, *attempt, arrived - *since);
        sends.lock().unwrap().push((ordinal, arrived));
        let response = format!(
            "HTTP/1.1 {status} Status\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
        );
        let _ = socket.write_all(response.as_bytes());
    }
}

fn gaps(sends: &[Instant]) -> Vec<Duration> {
    sends.windows(2).map(|pair| pair[1] - pair[0]).collect()
}

#[test]
fn a_head_answered_425_is_resent_on_a_doubling_delay_until_it_is_accepted() {
    let rule: Rule = Arc::new(|_, attempt, _| if attempt <= 6 { 425 } else { 202 });
    let mut relay = Relay::new(rule);
    let queue = relay.enqueue("too-early-backoff", 1);
    relay.work(queue.clone());
    let sends = relay.sends_of(1, 7);
    let floors = [25, 50, 100, 200, 400, 800].map(|ms| Duration::from_millis(ms * 4 / 5));
    for (gap, floor) in gaps(&sends).iter().zip(floors) {
        assert!(
            *gap >= floor,
            "gap {gap:?} under {floor:?}: {:?}",
            gaps(&sends)
        );
    }
    wait_until(
        || queue_request_paths(&queue).is_ok_and(|paths| paths.is_empty()),
        "the accepted request completes",
    );
}

#[test]
fn the_next_head_starts_from_the_first_delay() {
    let rule: Rule = Arc::new(|ordinal, attempt, _| match (ordinal, attempt) {
        (1, n) if n <= 5 => 425,
        (2, 1) => 425,
        _ => 202,
    });
    let mut relay = Relay::new(rule);
    let queue = relay.enqueue("too-early-next-head", 1);
    relay.enqueue("too-early-next-head", 2);
    relay.work(queue);
    let second = relay.sends_of(2, 2);
    let gap = second[1] - second[0];
    assert!(
        gap < Duration::from_millis(400),
        "the second head waited {gap:?}"
    );
}

/// A Stop waits 750ms behind a non-wait head the receiver refuses until 900ms: the head is resent
/// each interval while the Stop waits, so the Stop's reply reaches its caller in time.
#[test]
fn a_caller_waiting_behind_a_refused_head_gets_its_reply_inside_its_window() {
    let rule: Rule = Arc::new(|ordinal, _, since| match ordinal {
        1 if since < Duration::from_millis(900) => 425,
        _ => 202,
    });
    let mut relay = Relay::new(rule);
    let session = "too-early-waiting-caller";
    let queue = relay.enqueue(session, 1);
    relay.work(queue);
    let first = relay.sends_of(1, 1)[0];
    std::thread::sleep(Duration::from_millis(450).saturating_sub(first.elapsed()));
    let payload = json!({ "ordinal": 2 });
    let wait = Duration::from_millis(750);
    let reply = handoff_ordered_hook_event_response_with_timeout(
        &relay.endpoint,
        "claude",
        "Stop",
        session,
        payload,
        wait,
    );
    assert!(reply.is_ok(), "{reply:?}");
}

#[test]
fn a_head_refused_until_its_delivery_deadline_is_quarantined_with_a_marker() {
    let rule: Rule = Arc::new(|ordinal, _, _| if ordinal == 1 { 425 } else { 202 });
    let mut relay = Relay::new(rule);
    let session = "too-early-deadline";
    let queue = relay.enqueue(session, 1);
    let ingress = queue_ingress_paths(&queue).unwrap().remove(0);
    let mut request: Value = serde_json::from_slice(&std::fs::read(&ingress).unwrap()).unwrap();
    let deadline = Utc::now() + chrono::Duration::milliseconds(500);
    request["delivery_deadline"] = json!(deadline);
    std::fs::write(&ingress, serde_json::to_vec(&request).unwrap()).unwrap();
    let expires = Instant::now() + Duration::from_millis(500);
    relay.enqueue(session, 2);
    relay.work(queue.clone());
    relay.sends_of(2, 1);
    let refused = relay.sends_of(1, 1);
    assert!(
        refused.iter().all(|at| *at < expires),
        "sent after its deadline"
    );
    let quarantined = std::fs::read_dir(queue.join("quarantine")).unwrap();
    let quarantined = quarantined.flatten().map(|entry| entry.file_name());
    let corrupt = quarantined.filter(|name| name.to_string_lossy().ends_with(".corrupt"));
    assert_eq!(corrupt.count(), 1, "the expired head");
    let markers = std::fs::read_dir(failure_marker_dir("claude").unwrap())
        .unwrap()
        .count();
    assert_eq!(markers, 1);
}

/// Forty 64 KiB hooks nobody waits on queue behind a head refused for 1.2s: the backoff passes
/// read each reply window once while it is queued, not on every pass.
#[test]
fn a_backlog_behind_a_refused_head_has_each_reply_window_read_once() {
    let rule: Rule = Arc::new(|ordinal, _, since| match ordinal {
        1 if since < Duration::from_millis(1200) => 425,
        _ => 202,
    });
    let mut relay = Relay::new(rule);
    let session = "too-early-backlog";
    let queue = relay.enqueue(session, 1);
    let pad = "x".repeat(64 * 1024);
    for ordinal in 2..=41 {
        relay.enqueue_payload(session, json!({ "ordinal": ordinal, "pad": pad }));
    }
    relay.work(queue.clone());
    assert!(
        relay.sends_of(1, 6).len() >= 6,
        "the head was held over several passes"
    );
    relay.sends_of(41, 1);
    let reads = crate::services::claude_tui::hook_relay::too_early::tests::window_reads(&queue);
    assert!(reads.len() >= 40, "{reads:?}");
    assert!(reads.iter().all(|count| *count == 1), "{reads:?}");
}
