//! Another local program may connect to a test receiver's loopback port (a scanner's `GET /`, an
//! empty connect); every relay request is a `POST`, so a receiver skips any other connection.

use std::io::Read;
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

const RELAY_METHOD: &[u8] = b"POST ";

/// Whether `head`, the bytes read so far, already differs from every relay request.
pub(crate) fn not_a_relay(head: &[u8]) -> bool {
    let n = head.len().min(RELAY_METHOD.len());
    head[..n] != RELAY_METHOD[..n]
}

/// One relay request read to its body's end, or `None` for a connection that is not a relay,
/// which the caller skips. A relay that ends before its body panics with `ended`.
pub(crate) fn read_relay_request(socket: &mut impl Read, ended: &str) -> Option<Vec<u8>> {
    let mut request = Vec::new();
    let mut buffer = [0u8; 4096];
    loop {
        let read = socket.read(&mut buffer).expect("read relay request");
        if read == 0 && request.is_empty() {
            return None;
        }
        assert!(read > 0, "{ended}");
        request.extend_from_slice(&buffer[..read]);
        if not_a_relay(&request) {
            return None;
        }
        if let Some((start, len)) = body_bounds(&request)
            && request.len() >= start + len
        {
            return Some(request);
        }
    }
}

/// Accepts on a blocking `listener` until a connection carries a whole relay request, each read
/// bounded by `read_timeout`.
pub(crate) fn accept_relay_request(
    listener: &TcpListener,
    read_timeout: Duration,
    ended: &str,
) -> (TcpStream, Vec<u8>) {
    loop {
        let (mut socket, _) = listener.accept().expect("accept relay request");
        socket
            .set_read_timeout(Some(read_timeout))
            .expect("set relay read timeout");
        if let Some(request) = read_relay_request(&mut socket, ended) {
            return (socket, request);
        }
    }
}

fn body_bounds(request: &[u8]) -> Option<(usize, usize)> {
    let end = request.windows(4).position(|w| w == b"\r\n\r\n")?;
    let headers = std::str::from_utf8(&request[..end]).ok()?;
    let len = headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse::<usize>().ok())
            .flatten()
    })?;
    Some((end + 4, len))
}

#[test]
fn a_probe_or_an_empty_connect_is_skipped_and_a_relay_is_read_whole() {
    let probe = b"GET / HTTP/1.1\r\nHost: x\r\nUser-Agent: Go-http-client/1.1\r\n\r\n";
    assert_eq!(read_relay_request(&mut &probe[..], "ended"), None);
    assert_eq!(read_relay_request(&mut &b""[..], "ended"), None);
    let relay = b"POST /hooks/claude/Stop HTTP/1.1\r\nContent-Length: 2\r\n\r\n{}";
    assert_eq!(
        read_relay_request(&mut &relay[..], "ended").as_deref(),
        Some(&relay[..])
    );
    let cut = std::panic::catch_unwind(|| {
        read_relay_request(&mut &relay[..relay.len() - 1], "relay ended early")
    });
    assert!(cut.is_err(), "a relay cut before its body still fails");
}
