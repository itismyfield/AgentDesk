use std::time::{Duration, Instant};

use super::{
    HTTP_POST_DEFAULT_TIMEOUT, MAX_RESPONSE_BYTES, build_request_head,
    build_request_head_for_method, build_response, contains_ctl, find_subslice,
    parse_content_length, request_target,
};

#[derive(Debug)]
pub(super) enum HttpFailure {
    Deadline(String),
    Other(String),
}

impl From<String> for HttpFailure {
    fn from(message: String) -> Self {
        Self::Other(message)
    }
}

impl HttpFailure {
    pub(super) fn message(self) -> String {
        match self {
            Self::Deadline(message) | Self::Other(message) => message,
        }
    }

    fn deadline_response(self) -> String {
        match self {
            Self::Deadline(error) => serde_json::json!({
                "error": error,
                "deadline_exceeded": true,
            })
            .to_string(),
            Self::Other(error) => serde_json::json!({ "error": error }).to_string(),
        }
    }
}

fn response_deadline_exceeded() -> HttpFailure {
    HttpFailure::Deadline("response deadline exceeded".to_string())
}

fn response_io_failure(phase: &str, error: std::io::Error, deadline: Instant) -> HttpFailure {
    use std::io::ErrorKind::{TimedOut, WouldBlock};
    if matches!(error.kind(), TimedOut | WouldBlock) || Instant::now() >= deadline {
        response_deadline_exceeded()
    } else {
        HttpFailure::Other(format!("{phase}: {error}"))
    }
}

pub(super) fn invoke_localhost_before(
    url: &str,
    body: String,
    reserve: Duration,
    post: bool,
) -> String {
    use std::sync::mpsc::{self, RecvTimeoutError};

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
    let deadline = match crate::utils::bridge_deadline::current() {
        Some(armed) => armed
            .checked_sub(reserve)
            .unwrap_or(now)
            .min(default_deadline),
        None => default_deadline,
    };
    if now >= deadline {
        let op = if post { "http.post" } else { "http.get" };
        return HttpFailure::Deadline(format!("bridge deadline passed before {op} started"))
            .deadline_response();
    }

    // Let JS settle its cursor even if the last socket syscall outlives the work deadline.
    let (reply, result) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = reply.send(loopback_http_request_inner(
            &parsed,
            &body,
            || deadline,
            method,
        ));
    });
    match result.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => error.deadline_response(),
        Err(RecvTimeoutError::Timeout) => response_deadline_exceeded().deadline_response(),
        Err(RecvTimeoutError::Disconnected) => r#"{"error":"thread panic"}"#.to_string(),
    }
}

pub(super) fn loopback_http_request_inner(
    url: &url::Url,
    body: &str,
    work_deadline: impl FnOnce() -> Instant,
    method: &str,
) -> Result<String, HttpFailure> {
    use std::io::{Read, Write};

    let request_target = request_target(url)?;
    let authority = url.authority();
    if authority.is_empty() || contains_ctl(authority) {
        return Err(HttpFailure::Other("unsafe request authority".to_string()));
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
    if Instant::now() >= deadline {
        return Err(response_deadline_exceeded());
    }
    stream
        .write_all(head.as_bytes())
        .and_then(|()| stream.write_all(body.as_bytes()))
        .and_then(|()| stream.flush())
        .map_err(|e| response_io_failure("write request", e, deadline))?;

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
            return Err(response_deadline_exceeded());
        }
        match stream.read(&mut buf) {
            Ok(_) if Instant::now() >= deadline => return Err(response_deadline_exceeded()),
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&buf[..n]);
                if raw.len() > MAX_RESPONSE_BYTES {
                    return Err(HttpFailure::Other("response exceeds 8 MiB cap".to_string()));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(response_io_failure("read response", e, deadline)),
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
    if addrs.is_empty() {
        return Err(HttpFailure::Other(
            "no socket address for target".to_string(),
        ));
    }
    let mut last_error = String::new();
    for addr in addrs {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(HttpFailure::Deadline(if last_error.is_empty() {
                "connect deadline exceeded".to_string()
            } else {
                format!("connect deadline exceeded (last attempt: {last_error})")
            }));
        }
        match std::net::TcpStream::connect_timeout(addr, remaining) {
            Ok(stream) => return Ok(stream),
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut || Instant::now() >= deadline => {
                return Err(HttpFailure::Deadline(format!(
                    "connect deadline exceeded (last attempt: {e})"
                )));
            }
            Err(e) => last_error = format!("connect {addr}: {e}"),
        }
    }
    Err(HttpFailure::Other(last_error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Error, ErrorKind};

    #[test]
    fn loopback_timeout_kinds_are_deadlines_before_the_cutoff() {
        for kind in [ErrorKind::WouldBlock, ErrorKind::TimedOut] {
            let deadline = Instant::now() + Duration::from_secs(5);
            let error = response_io_failure("read response", Error::from(kind), deadline);
            assert!(
                matches!(error, HttpFailure::Deadline(message) if message == "response deadline exceeded"),
                "{kind:?}",
            );
        }
    }
}
