use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use super::{
    HTTP_POST_DEFAULT_TIMEOUT, MAX_RESPONSE_BYTES, build_request_head,
    build_request_head_for_method, build_response, contains_ctl, find_subslice,
    parse_content_length, request_target,
};

#[derive(Debug)]
pub(super) struct HttpFailure {
    message: String,
    deadline_started: Option<bool>,
    io: Option<std::io::Error>,
}

impl From<String> for HttpFailure {
    fn from(message: String) -> Self {
        Self {
            message,
            deadline_started: None,
            io: None,
        }
    }
}

impl HttpFailure {
    fn deadline(message: String, started: bool) -> Self {
        Self {
            message,
            deadline_started: Some(started),
            io: None,
        }
    }

    pub(super) fn message(self) -> String {
        self.message
    }

    fn response(self) -> String {
        let mut result = serde_json::json!({ "error": self.message });
        if let Some(started) = self.deadline_started {
            result["deadline_exceeded"] = true.into();
            result["request_started"] = started.into();
        }
        if let Some(error) = self.io {
            result["io_error"] = serde_json::json!({
                "kind": format!("{:?}", error.kind()),
                "raw_os_error": error.raw_os_error()
            });
        }
        result.to_string()
    }
}

fn response_deadline_exceeded(started: bool) -> HttpFailure {
    HttpFailure::deadline("response deadline exceeded".to_string(), started)
}

fn io_failure(phase: &str, error: std::io::Error, deadline: Instant, started: bool) -> HttpFailure {
    use std::io::ErrorKind::{TimedOut, WouldBlock};
    let timed_out = matches!(error.kind(), TimedOut | WouldBlock) || Instant::now() >= deadline;
    let mut failure = if !timed_out {
        HttpFailure::from(format!("{phase}: {error}"))
    } else if started {
        response_deadline_exceeded(true)
    } else {
        HttpFailure::deadline(
            format!("connect deadline exceeded (last attempt: {error})"),
            false,
        )
    };
    failure.io = Some(error);
    failure
}

pub(super) fn invoke_localhost_before(
    url: &str,
    body: String,
    reserve: Duration,
    post: bool,
) -> String {
    let parsed = match url::Url::parse(url) {
        Ok(parsed)
            if parsed.scheme() == "http"
                && crate::utils::loopback_url::is_loopback_url(url, None) =>
        {
            parsed
        }
        _ => return r#"{"error":"only http localhost allowed"}"#.to_string(),
    };
    let method = if post { "POST" } else { "GET" };
    let now = Instant::now();
    let default_deadline = now + HTTP_POST_DEFAULT_TIMEOUT;
    let deadline = match crate::utils::bridge_deadline::work_deadline(reserve) {
        Some(work) => work.min(default_deadline),
        None => default_deadline,
    };
    if Instant::now() >= deadline {
        let op = if post { "http.post" } else { "http.get" };
        return HttpFailure::deadline(format!("bridge deadline passed before {op} started"), false)
            .response();
    }

    // The timeout and first write race for the same permit. A reported
    // unstarted request cannot begin after the caller has returned.
    let start_claimed = Arc::new(AtomicBool::new(false));
    let worker_claim = start_claimed.clone();
    let (reply, result) = mpsc::channel();
    std::thread::spawn(move || {
        let outcome = loopback_http_request_inner(
            &parsed,
            &body,
            || deadline,
            method,
            || !worker_claim.swap(true, Ordering::AcqRel),
        );
        if let Err(failure) = &outcome
            && let Some(error) = &failure.io
            && Instant::now() >= deadline
        {
            tracing::debug!(kind = ?error.kind(), raw_os_error = ?error.raw_os_error(), "loopback I/O failed at the work deadline");
        }
        let _ = reply.send(outcome);
    });
    match result.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => error.response(),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            response_deadline_exceeded(start_claimed.swap(true, Ordering::AcqRel)).response()
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => r#"{"error":"thread panic"}"#.to_string(),
    }
}

pub(super) fn loopback_http_request_inner(
    url: &url::Url,
    body: &str,
    work_deadline: impl FnOnce() -> Instant,
    method: &str,
    claim_start: impl FnOnce() -> bool,
) -> Result<String, HttpFailure> {
    use std::io::{Read, Write};

    let request_target = request_target(url)?;
    let authority = url.authority();
    if authority.is_empty() || contains_ctl(authority) {
        return Err("unsafe request authority".to_string().into());
    }

    let addrs = url
        .socket_addrs(|| Some(80))
        .map_err(|e| format!("resolve {authority}: {e}"))?;

    // Total-deadline anchor: the per-read socket timeout below bounds each
    // `read` syscall, NOT the whole exchange — a peer dribbling one byte per
    // read window would extend the call indefinitely. Checking elapsed time
    // against this deadline before every read bounds the worst case at
    // `timeout + one read window` (≤ 2×timeout) without re-arming
    // `set_read_timeout` mid-stream, which is the exact syscall-after-peer-
    // close that EINVALs on macOS (see the #4251 note on
    // `loopback_http_post`). It also caps the combined connect attempts below.
    let deadline = work_deadline();

    let mut stream = connect_first_reachable(&addrs, deadline)?;

    // Set the socket timeouts exactly once, immediately after connect while
    // the fd is guaranteed valid, and never touch them again (see the #4251
    // note on `loopback_http_post`). std rejects a zero-duration timeout with
    // EINVAL, so clamp up to at least 1ms.
    let socket_timeout = deadline
        .saturating_duration_since(Instant::now())
        .max(Duration::from_millis(1));
    stream
        .set_read_timeout(Some(socket_timeout))
        .map_err(|e| format!("set_read_timeout: {e}"))?;
    stream
        .set_write_timeout(Some(socket_timeout))
        .map_err(|e| format!("set_write_timeout: {e}"))?;

    let head = if method == "POST" {
        build_request_head(&request_target, authority, body.len())
    } else {
        build_request_head_for_method(&request_target, authority, body.len(), method)
    };
    if Instant::now() >= deadline || !claim_start() || Instant::now() >= deadline {
        return Err(response_deadline_exceeded(false));
    }
    let write_failure = |error| io_failure("write request", error, deadline, true);
    stream.write_all(head.as_bytes()).map_err(write_failure)?;
    if Instant::now() >= deadline {
        return Err(response_deadline_exceeded(true));
    }
    stream.write_all(body.as_bytes()).map_err(write_failure)?;
    if Instant::now() >= deadline {
        return Err(response_deadline_exceeded(true));
    }
    stream.flush().map_err(write_failure)?;

    let mut raw: Vec<u8> = Vec::with_capacity(1024);
    let mut buf = [0u8; 8192];
    let mut header_end: Option<usize> = None;
    let mut content_length: Option<usize> = None;
    loop {
        if header_end.is_none() {
            if let Some(pos) = find_subslice(&raw, b"\r\n\r\n") {
                header_end = Some(pos + 4);
                content_length = parse_content_length(&raw[..pos]);
            }
        }
        // Early exit once the declared body has fully arrived, so a peer that
        // ignores `Connection: close` cannot make us block until the read
        // timeout.
        if let (Some(he), Some(cl)) = (header_end, content_length) {
            if raw.len() >= he.saturating_add(cl) {
                return build_response(&raw, he, Some(cl)).map_err(HttpFailure::from);
            }
        }
        if Instant::now() >= deadline {
            return Err(response_deadline_exceeded(true));
        }
        match stream.read(&mut buf) {
            Ok(_) if Instant::now() >= deadline => return Err(response_deadline_exceeded(true)),
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&buf[..n]);
                if raw.len() > MAX_RESPONSE_BYTES {
                    return Err("response exceeds 8 MiB cap".to_string().into());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted && Instant::now() < deadline => {
                continue;
            }
            Err(e) => return Err(io_failure("read response", e, deadline, true)),
        }
    }
    let he =
        header_end.ok_or_else(|| "incomplete response: missing header terminator".to_string())?;
    build_response(&raw, he, content_length).map_err(HttpFailure::from)
}

/// Try every resolved address until one connects, all under the caller's
/// total deadline (codex #4391 r3-1: macOS resolves `localhost` to `::1`
/// before `127.0.0.1`; taking only the first address made an IPv4-only
/// loopback server unreachable — ureq looped the list too).
pub(super) fn connect_first_reachable(
    addrs: &[std::net::SocketAddr],
    deadline: Instant,
) -> Result<std::net::TcpStream, HttpFailure> {
    let mut last: Option<HttpFailure> = None;
    for addr in addrs {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            let mut failure = HttpFailure::deadline("connect deadline exceeded".to_string(), false);
            if let Some(previous) = last {
                failure.message = format!(
                    "connect deadline exceeded (last attempt: {})",
                    previous.message
                );
                failure.io = previous.io;
            }
            return Err(failure);
        }
        match std::net::TcpStream::connect_timeout(addr, remaining) {
            Ok(stream) => return Ok(stream),
            Err(error) => {
                let failure = io_failure(&format!("connect {addr}"), error, deadline, false);
                if failure.deadline_started.is_some() {
                    return Err(failure);
                }
                last = Some(failure);
            }
        }
    }
    Err(last.unwrap_or_else(|| "no socket address for target".to_string().into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Error, ErrorKind};

    #[test]
    fn loopback_timeout_kinds_are_deadlines_before_the_cutoff() {
        for (kind, timeout_kind) in [
            (ErrorKind::WouldBlock, true),
            (ErrorKind::TimedOut, true),
            (ErrorKind::ConnectionReset, false),
            (ErrorKind::BrokenPipe, false),
        ] {
            for past in [false, true] {
                let now = Instant::now();
                let deadline = if past {
                    now
                } else {
                    now + Duration::from_secs(5)
                };
                let error = io_failure("read response", Error::from(kind), deadline, true);
                let response: serde_json::Value = serde_json::from_str(&error.response()).unwrap();
                let expired = timeout_kind || past;
                assert_eq!(
                    response["deadline_exceeded"].as_bool(),
                    expired.then_some(true),
                    "{kind:?}, past={past}"
                );
                assert_eq!(
                    response["request_started"].as_bool(),
                    expired.then_some(true)
                );
                assert_eq!(response["io_error"]["kind"], format!("{kind:?}"));
            }
        }
    }

    #[test]
    fn loopback_rejects_before_write_when_request_start_is_revoked() {
        use std::io::Read;
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = url::Url::parse(&format!(
            "http://{}/no-post",
            listener.local_addr().unwrap()
        ))
        .unwrap();
        let failure = loopback_http_request_inner(
            &url,
            "{}",
            || Instant::now() + Duration::from_secs(2),
            "POST",
            || false,
        )
        .unwrap_err();
        let result: serde_json::Value = serde_json::from_str(&failure.response()).unwrap();
        assert_eq!(result["request_started"], false);
        listener.set_nonblocking(true).unwrap();
        let (mut accepted, _) = listener.accept().unwrap();
        accepted
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        assert_eq!(accepted.read(&mut [0]).unwrap(), 0);
    }
}
