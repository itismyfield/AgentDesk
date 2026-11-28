use rquickjs::{Ctx, Function, Object, Result as JsResult};
use std::time::{Duration, Instant};

#[path = "http_ops/loopback.rs"]
mod loopback;
use loopback::{HttpFailure, invoke_localhost_before, loopback_http_request_inner};

// ── HTTP ops ────────────────────────────────────────────────────
//
// agentdesk.http.post(url, body) → response_string
// Synchronous HTTP POST for localhost API calls from policy JS.
// Only allows loopback targets for security.

/// Convert a panic payload (`Box<dyn Any + Send>`) into a short readable
/// description so the JS caller gets `{"error":"ureq panic: ..."}` instead
/// of a bare `{"error":"thread panic"}` with the real reason lost to stderr.
fn describe_panic_payload(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}

/// JSON-string-escape a value so the synthesized `{"error":"..."}` payload
/// stays parseable even when the underlying message contains quotes or
/// backslashes (the older inline `format!` produced invalid JSON for ureq
/// errors that embed `\n` or `"`).
fn escape_for_json(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    for ch in value.chars() {
        match ch {
            '\\' | '"' => {
                out.push('\\');
                out.push(ch);
            }
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

/// Default ureq request timeout for synchronous localhost POSTs. Clamped to
/// the bridge-op deadline when one is armed (#2378).
const HTTP_POST_DEFAULT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Choose the ureq timeout: the smaller of the default and the remaining
/// bridge-op budget. Returns `Err(message)` if the deadline has already
/// passed so the policy can short-circuit instead of issuing a doomed
/// request that would block the runtime lock past the deadline.
fn resolve_http_post_timeout() -> Result<std::time::Duration, String> {
    match crate::utils::bridge_deadline::remaining() {
        None => Ok(HTTP_POST_DEFAULT_TIMEOUT),
        Some(remaining) if remaining.is_zero() => {
            Err("bridge deadline passed before http.post started".to_string())
        }
        Some(remaining) => Ok(remaining.min(HTTP_POST_DEFAULT_TIMEOUT)),
    }
}

/// Upper bound on the response body we buffer from a localhost POST. Our own
/// routes reply with small JSON; this only guards against a runaway peer.
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

fn invoke_localhost_post(url: &str, body_json: &str) -> String {
    if !crate::utils::loopback_url::is_loopback_url(url, None) {
        return r#"{"error":"only localhost allowed"}"#.to_string();
    }
    // #2378: bound the request timeout by the current eval deadline so a
    // hot-reloaded policy can never hold the runtime lock longer than the
    // deadline budget while waiting for a localhost POST to return.
    let timeout = match resolve_http_post_timeout() {
        Ok(d) => d,
        Err(message) => {
            return format!(r#"{{"error":"{}"}}"#, escape_for_json(&message));
        }
    };
    let url_owned = url.to_string();
    let body_owned = body_json.to_string();
    // Run on a dedicated thread to avoid blocking the tokio I/O driver.
    // The request is synchronous — if issued directly on a tokio worker it can
    // self-deadlock when the target is our own HTTP server (the worker blocks
    // on recv while no other worker is available to handle the inbound
    // request).
    let handle = std::thread::spawn(move || match url::Url::parse(&url_owned) {
        // The dcserver is plain http, so every real policy call takes this
        // panic-free, self-managed path (see `loopback_http_post`).
        Ok(parsed) if parsed.scheme() == "http" => {
            loopback_http_post(&parsed, &body_owned, timeout)
        }
        // https loopback is unused by the dcserver but permitted by
        // `is_loopback_url`; keep the legacy ureq path so TLS targets still
        // work if a policy ever needs one.
        _ => ureq_localhost_post(&url_owned, &body_owned, timeout),
    });
    handle
        .join()
        .unwrap_or_else(|_| r#"{"error":"thread panic"}"#.to_string())
}

// Read-only policy preflight for the plain-HTTP dcserver. Reuse the POST
// transport's bounded, proxy-free, redirect-free socket path (#5714).
fn invoke_localhost_get(url: &str) -> String {
    let parsed = match url::Url::parse(url) {
        Ok(parsed)
            if parsed.scheme() == "http"
                && crate::utils::loopback_url::is_loopback_url(url, None) =>
        {
            parsed
        }
        _ => return r#"{"error":"only http localhost allowed"}"#.to_string(),
    };
    let timeout = match resolve_http_post_timeout() {
        Ok(timeout) => timeout,
        Err(message) => return format!(r#"{{"error":"{}"}}"#, escape_for_json(&message)),
    };
    std::thread::spawn(move || {
        match loopback_http_request_inner(&parsed, "", || Instant::now() + timeout, "GET", || true)
        {
            Ok(body) => body,
            Err(error) => format!(r#"{{"error":"{}"}}"#, escape_for_json(&error.message())),
        }
    })
    .join()
    .unwrap_or_else(|_| r#"{"error":"thread panic"}"#.to_string())
}

/// Panic-free blocking HTTP/1.1 POST to a loopback target.
///
/// #4251 root fix. ureq-2.12.1 resets the socket read timeout
/// (`set_read_timeout`, a `setsockopt` syscall) twice while handling a
/// response: once per `fill_buf` while reading the response headers
/// (`stream.rs` `DeadlineStream::fill_buf`) and again when it returns the
/// stream to its connection pool (`stream.rs` `reset()` →
/// `set_read_timeout(None)`). On macOS that syscall fails with EINVAL
/// (os error 22) as soon as the loopback peer — our own axum server — has
/// closed its side of the socket. ureq then surfaces it two different ways,
/// which are exactly the two symptoms in #4251:
///   * header phase → transport error
///     `Network Error: Error encountered in a header: Invalid argument`;
///   * buffered-body phase → `read_exact(..).expect("failed to read exact
///     buffer length from stream")` PANIC at ureq `response.rs:403`.
/// Neither is a tainted header value (the session key is url-encoded into the
/// path; the only request header is a constant `Content-Type`) — both are the
/// same `setsockopt` EINVAL. We fix it at the source by owning the socket: the
/// read/write timeout is set exactly once, right after `connect` while the fd
/// is guaranteed valid, and never reset — so the failing syscall never runs
/// and neither symptom can occur.
fn loopback_http_post(url: &url::Url, body: &str, timeout: std::time::Duration) -> String {
    match loopback_http_post_inner(url, body, timeout) {
        Ok(response_body) => response_body,
        Err(message) => format!(r#"{{"error":"{}"}}"#, escape_for_json(&message)),
    }
}

fn loopback_http_post_inner(
    url: &url::Url,
    body: &str,
    timeout: std::time::Duration,
) -> Result<String, String> {
    loopback_http_request_inner(url, body, || Instant::now() + timeout, "POST", || true)
        .map_err(HttpFailure::message)
}

/// Build the request line target (`path[?query]`) and fail closed if it
/// carries any byte that could break the request line or smuggle a second
/// request. `url::Url` already percent-encodes control characters, so this is
/// defense in depth for the exact #3007/#4251 worry (a colon/control char
/// reaching the wire) rather than a reachable-in-practice path.
fn request_target(url: &url::Url) -> Result<String, String> {
    sanitize_request_target(url.path(), url.query())
}

fn sanitize_request_target(path: &str, query: Option<&str>) -> Result<String, String> {
    let mut target = if path.is_empty() {
        "/".to_string()
    } else {
        path.to_string()
    };
    if let Some(query) = query {
        target.push('?');
        target.push_str(query);
    }
    if contains_ctl(&target) {
        return Err("unsafe request target".to_string());
    }
    Ok(target)
}

fn contains_ctl(s: &str) -> bool {
    s.bytes().any(|b| b < 0x20 || b == 0x7f || b == b' ')
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Request head for the loopback POST. Deliberately `HTTP/1.0`: a compliant
/// server must not reply with `Transfer-Encoding: chunked` to a 1.0 client,
/// so every well-formed response is either `Content-Length`-delimited or
/// close-delimited — the two framings the read loop understands. This keeps
/// the client free of a chunked decoder (parsing surface we chose not to
/// grow; a rogue chunked reply is rejected fail-closed in `build_response`).
fn build_request_head(request_target: &str, authority: &str, body_len: usize) -> String {
    build_request_head_for_method(request_target, authority, body_len, "POST")
}

fn build_request_head_for_method(
    request_target: &str,
    authority: &str,
    body_len: usize,
    method: &str,
) -> String {
    format!(
        "{method} {request_target} HTTP/1.0\r\n\
         Host: {authority}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {body_len}\r\n\
         Connection: close\r\n\r\n"
    )
}

/// Case-insensitive check for `Transfer-Encoding: chunked` in the response
/// header block. We never decode chunked framing (see [`build_request_head`]);
/// returning the raw framing bytes as a "body" would hand policy JS silent
/// garbage, so it is rejected with an explicit error instead.
fn response_is_chunked(header_block: &[u8]) -> bool {
    let text = String::from_utf8_lossy(header_block);
    for line in text.split("\r\n") {
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("transfer-encoding")
                && value.to_ascii_lowercase().contains("chunked")
            {
                return true;
            }
        }
    }
    false
}

fn parse_content_length(header_block: &[u8]) -> Option<usize> {
    let text = String::from_utf8_lossy(header_block);
    for line in text.split("\r\n") {
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                return value.trim().parse::<usize>().ok();
            }
        }
    }
    None
}

fn parse_status_code(header_block: &[u8]) -> Result<u16, String> {
    let text = String::from_utf8_lossy(header_block);
    let status_line = text.split("\r\n").next().unwrap_or_default();
    status_line
        .split_whitespace()
        .nth(1)
        .and_then(|token| token.parse::<u16>().ok())
        .ok_or_else(|| format!("malformed status line: {status_line:?}"))
}

fn build_response(
    raw: &[u8],
    header_end: usize,
    content_length: Option<usize>,
) -> Result<String, String> {
    let status = parse_status_code(&raw[..header_end])?;
    if response_is_chunked(&raw[..header_end]) {
        return Err(
            "chunked transfer encoding not supported by the loopback client \
             (HTTP/1.0 request forbids it from compliant servers)"
                .to_string(),
        );
    }
    let body_slice = match content_length {
        Some(cl) => {
            let end = header_end.saturating_add(cl);
            // codex #4391 r3-2: a premature EOF under a declared
            // Content-Length is a transport error; silently returning the
            // truncated prefix could hand policy JS a misleading "success".
            if raw.len() < end {
                return Err(format!(
                    "truncated response: Content-Length {cl} but only {} body bytes arrived",
                    raw.len() - header_end
                ));
            }
            &raw[header_end..end]
        }
        None => &raw[header_end..],
    };
    let body = String::from_utf8_lossy(body_slice).into_owned();
    if (200..300).contains(&status) {
        if body.trim().is_empty() {
            Ok("{}".to_string())
        } else {
            Ok(body)
        }
    } else if body.trim().is_empty() {
        Err(format!("status code {status}"))
    } else {
        // The route replies with a JSON error body (`{"ok":false,...}`); pass
        // it through verbatim so the policy sees the real reason instead of a
        // synthesized transport error.
        Ok(body)
    }
}

/// Legacy ureq path, retained only for https loopback targets (the dcserver
/// itself is plain http, so no real policy call reaches here). Kept behind
/// `catch_unwind` because ureq-2.12.1 can still panic on the response read
/// path (#2098).
#[allow(clippy::result_large_err)]
fn ureq_localhost_post(url: &str, body: &str, timeout: std::time::Duration) -> String {
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let request = ureq::AgentBuilder::new()
            .timeout(timeout)
            .build()
            .post(url)
            .set("Content-Type", "application/json");
        request.send_string(body).map(|resp| {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                resp.into_string().unwrap_or_else(|_| "{}".to_string())
            }))
            .unwrap_or_else(|payload| {
                format!(
                    r#"{{"error":"ureq panic: {}"}}"#,
                    escape_for_json(&describe_panic_payload(payload))
                )
            })
        })
    }));
    match outcome {
        Ok(Ok(response_body)) => response_body,
        Ok(Err(err)) => format!(r#"{{"error":"{}"}}"#, escape_for_json(&err.to_string())),
        Err(payload) => format!(
            r#"{{"error":"ureq panic: {}"}}"#,
            escape_for_json(&describe_panic_payload(payload))
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::loopback::connect_first_reachable;
    use super::*;
    use crate::engine::loader::{
        ArmedDeadline, install_policy_interrupt_handler, new_eval_deadline_slot,
    };
    use crate::test_support::wait::await_within;
    use crate::utils::bridge_deadline::ScopedBridgeDeadline;
    use rquickjs::{Context, Runtime};
    use serde_json::{Value, json};
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::{Duration, Instant};
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    #[test]
    fn policy_http_get_uses_read_only_loopback_transport_and_js_registration() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                socket.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            let request = String::from_utf8(request).unwrap();
            assert!(request.starts_with("GET /api/cluster/nodes HTTP/1.0\r\n"));
            assert!(request.contains("Content-Length: 0\r\n"));
            socket
                .write_all(&http_response("200 OK", r#"{"nodes":[]}"#, true))
                .unwrap();
        });
        let runtime = rquickjs::Runtime::new().unwrap();
        let context = rquickjs::Context::full(&runtime).unwrap();
        context.with(|ctx| {
            ctx.eval::<(), _>("var agentdesk = {};").unwrap();
            register_http_ops(&ctx).unwrap();
            let count: i32 = ctx
                .eval(format!(
                    "agentdesk.http.get('http://127.0.0.1:{port}/api/cluster/nodes').nodes.length"
                ))
                .unwrap();
            assert_eq!(count, 0);
        });
        server.join().unwrap();
    }

    #[test]
    fn policy_http_get_rejects_remote_targets_and_never_follows_redirects() {
        assert!(
            invoke_localhost_get("http://100.64.0.1:8791/api/cluster/nodes")
                .contains("only http localhost allowed")
        );
        assert!(invoke_localhost_get("file:///etc/passwd").contains("only http localhost allowed"));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let target = listener.local_addr().unwrap();
        let response = format!(
            "HTTP/1.0 302 Found\r\nLocation: http://{target}/redirected\r\nContent-Length: 2\r\n\r\n{{}}"
        );
        let port = spawn_oneshot_server(response.into_bytes(), Duration::ZERO);
        assert_eq!(
            invoke_localhost_get(&format!("http://127.0.0.1:{port}/api/cluster/nodes")),
            "{}"
        );
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
        );
    }

    /// Accept exactly one connection, drain the request, write `response`,
    /// keep the socket open for `keep_open`, then drop it (FIN). This mimics
    /// our axum server replying and then closing the loopback connection —
    /// the condition under which ureq-2.12.1 hit the #4251 EINVAL.
    fn spawn_oneshot_server(response: Vec<u8>, keep_open: Duration) -> u16 {
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral test server");
        let port = listener.local_addr().expect("test server addr").port();
        std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let _ = sock.set_read_timeout(Some(Duration::from_millis(500)));
                let mut req = Vec::new();
                let mut buf = [0u8; 4096];
                loop {
                    match sock.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                    if let Some(pos) = find_subslice(&req, b"\r\n\r\n") {
                        let body_len = parse_content_length(&req[..pos]).unwrap_or(0);
                        if req.len() >= pos + 4 + body_len {
                            break;
                        }
                    }
                }
                let _ = sock.write_all(&response);
                let _ = sock.flush();
                std::thread::sleep(keep_open);
                // drop(sock) closes the connection (FIN).
            }
        });
        port
    }

    /// Like `spawn_oneshot_server`, but drains the FULL request (headers +
    /// declared body) before replying and signals end-of-response with a
    /// write-side FIN (`shutdown(Write)`) while keeping the socket alive
    /// briefly. Dropping a socket with unread request bytes emits RST, which
    /// races ahead of the buffered response and turns a deterministic
    /// EOF-path test flaky ("connection reset by peer").
    fn spawn_draining_oneshot_server(response: Vec<u8>) -> u16 {
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral test server");
        let port = listener.local_addr().expect("test server addr").port();
        std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let _ = sock.set_read_timeout(Some(Duration::from_millis(500)));
                let mut req: Vec<u8> = Vec::new();
                let mut buf = [0u8; 4096];
                loop {
                    match sock.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                    if let Some(pos) = find_subslice(&req, b"\r\n\r\n") {
                        let body_len = parse_content_length(&req[..pos]).unwrap_or(0);
                        if req.len() >= pos + 4 + body_len {
                            break;
                        }
                    }
                }
                let _ = sock.write_all(&response);
                let _ = sock.flush();
                let _ = sock.shutdown(std::net::Shutdown::Write);
                std::thread::sleep(Duration::from_millis(500));
            }
        });
        port
    }

    fn http_response(status_line: &str, body: &str, connection_close: bool) -> Vec<u8> {
        let conn = if connection_close {
            "Connection: close\r\n"
        } else {
            ""
        };
        format!(
            "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\n{conn}Content-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    /// #4251: the exact production shape — server replies then closes the
    /// loopback connection. The self-managed client must read the full body
    /// and return it, with no panic and no EINVAL.
    #[test]
    fn loopback_http_post_returns_body_when_server_closes_after_response() {
        let body = r#"{"ok":true,"accepted":true,"posted":false}"#;
        let port = spawn_oneshot_server(http_response("200 OK", body, true), Duration::ZERO);
        let url = format!("http://127.0.0.1:{port}/api/sessions/claude%2Fhost%3Asess/idle-recap");
        let out = invoke_localhost_post(&url, "{}");
        let parsed: serde_json::Value =
            serde_json::from_str(&out).unwrap_or_else(|e| panic!("response not JSON: {e}: {out}"));
        assert_eq!(
            parsed.get("ok").and_then(|v| v.as_bool()),
            Some(true),
            "out={out}"
        );
        assert_eq!(
            parsed.get("accepted").and_then(|v| v.as_bool()),
            Some(true),
            "out={out}"
        );
    }

    /// Non-2xx replies carry the route's JSON error body; it is passed through
    /// verbatim so the policy sees `ok:false` and the real reason.
    #[test]
    fn loopback_http_post_passes_through_non_2xx_json_body() {
        let body = r#"{"ok":false,"error":"session not found"}"#;
        let port = spawn_oneshot_server(http_response("404 Not Found", body, true), Duration::ZERO);
        let url = format!("http://127.0.0.1:{port}/api/sessions/claude%2Fx%3Ay/idle-recap");
        let out = invoke_localhost_post(&url, "{}");
        let parsed: serde_json::Value =
            serde_json::from_str(&out).unwrap_or_else(|e| panic!("response not JSON: {e}: {out}"));
        assert_eq!(
            parsed.get("ok").and_then(|v| v.as_bool()),
            Some(false),
            "out={out}"
        );
        assert_eq!(
            parsed.get("error").and_then(|v| v.as_str()),
            Some("session not found"),
            "out={out}"
        );
    }

    /// MUTATION GUARD (Content-Length early-exit). The server replies with a
    /// Content-Length body but *keeps the connection open* for 1.5s before
    /// closing, and sends no `Connection: close`. With the early-exit the
    /// client returns as soon as the declared body has arrived (~ms). Remove
    /// the early-exit block and read-to-EOF blocks until the server closes,
    /// blowing the sub-800ms budget below.
    #[test]
    fn loopback_http_post_returns_on_content_length_before_server_close() {
        let body = r#"{"ok":true,"posted":false,"skipped":true}"#;
        let port = spawn_oneshot_server(
            http_response("200 OK", body, false),
            Duration::from_millis(1500),
        );
        let url = format!("http://127.0.0.1:{port}/api/sessions/claude%2Fh%3As/idle-recap");
        let start = Instant::now();
        let out = invoke_localhost_post(&url, "{}");
        let elapsed = start.elapsed();
        let parsed: serde_json::Value =
            serde_json::from_str(&out).unwrap_or_else(|e| panic!("response not JSON: {e}: {out}"));
        assert_eq!(
            parsed.get("ok").and_then(|v| v.as_bool()),
            Some(true),
            "out={out}"
        );
        assert!(
            elapsed < Duration::from_millis(800),
            "Content-Length early-exit guard missing: waited {elapsed:?} for the server to close \
             instead of returning once the declared body arrived; out={out}"
        );
    }

    /// Accept one connection, send headers declaring a large body, then
    /// dribble one byte per `gap` for up to `lifetime`, then drop. Each gap is
    /// far below the per-read socket timeout, so only a TOTAL deadline stops
    /// the exchange early.
    fn spawn_dribble_server(gap: Duration, lifetime: Duration) -> u16 {
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral dribble server");
        let port = listener.local_addr().expect("dribble server addr").port();
        std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let _ = sock.set_read_timeout(Some(Duration::from_millis(500)));
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf);
                let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000000\r\n\r\n");
                let _ = sock.flush();
                let start = Instant::now();
                while start.elapsed() < lifetime {
                    if sock.write_all(b"x").and_then(|()| sock.flush()).is_err() {
                        break;
                    }
                    std::thread::sleep(gap);
                }
            }
        });
        port
    }

    /// MUTATION GUARD (total deadline). The per-read socket timeout only
    /// bounds each `read` syscall; a peer dribbling a byte per 50ms never
    /// trips it. Remove the `deadline` check in the read loop and this call
    /// runs for the server's whole 3s lifetime instead of erroring at ~400ms,
    /// blowing both asserts below.
    #[test]
    fn loopback_http_post_enforces_total_deadline_against_dribbling_peer() {
        let port = spawn_dribble_server(Duration::from_millis(50), Duration::from_secs(3));
        let url = url::Url::parse(&format!(
            "http://127.0.0.1:{port}/api/sessions/claude%2Fh%3As/idle-recap"
        ))
        .expect("test url");
        let start = Instant::now();
        let result = loopback_http_post_inner(&url, "{}", Duration::from_millis(400));
        let elapsed = start.elapsed();
        assert!(
            matches!(&result, Err(message) if message.contains("deadline exceeded")),
            "expected total-deadline error, got {result:?}"
        );
        assert!(
            elapsed < Duration::from_millis(1500),
            "total deadline missing: dribbling peer held the call for {elapsed:?}"
        );
    }

    /// MUTATION GUARD (streaming response cap). The 8 MiB cap is enforced
    /// while the body streams in, so an oversized reply is cut off mid-flight:
    /// the client errors and closes, and the peer cannot deliver its full
    /// payload. Remove the in-loop cap check and the client buffers all 64 MiB
    /// to EOF, failing both asserts.
    #[test]
    fn loopback_http_post_caps_oversized_response_while_streaming() {
        let total: usize = 64 * 1024 * 1024;
        let sent = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let sent_in_server = sent.clone();
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral cap server");
        let port = listener.local_addr().expect("cap server addr").port();
        let server = std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let _ = sock.set_read_timeout(Some(Duration::from_millis(500)));
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf);
                let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {total}\r\n\r\n");
                if sock.write_all(head.as_bytes()).is_err() {
                    return;
                }
                let chunk = vec![b'x'; 256 * 1024];
                let mut written = 0usize;
                while written < total {
                    let n = chunk.len().min(total - written);
                    if sock.write_all(&chunk[..n]).is_err() {
                        break;
                    }
                    written += n;
                    sent_in_server.store(written, std::sync::atomic::Ordering::SeqCst);
                }
            }
        });
        let url = url::Url::parse(&format!(
            "http://127.0.0.1:{port}/api/sessions/claude%2Fh%3As/idle-recap"
        ))
        .expect("test url");
        let result = loopback_http_post_inner(&url, "{}", Duration::from_secs(10));
        assert!(
            matches!(&result, Err(message) if message.contains("exceeds 8 MiB cap")),
            "expected streaming cap error, got truncated-or-ok result: {:?}",
            result.as_ref().map(|s| s.len())
        );
        crate::test_support::wait::join_within(server, crate::test_support::wait::WAIT_CAP);
        let delivered = sent.load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            delivered < total,
            "client kept reading to EOF: server pushed all {total} bytes"
        );
    }

    /// MUTATION GUARD (codex #4391 r3-1): `localhost` may resolve to `::1` first, so try every
    /// address. The closed first port refuses at once on unix, after ~2 s on Windows.
    #[test]
    fn connect_first_reachable_falls_through_to_second_address() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind reachable server");
        let good = listener.local_addr().expect("addr");
        // Reserve-and-drop a port so the first candidate refuses connections.
        let closed = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind probe");
            let a = l.local_addr().expect("probe addr");
            drop(l);
            a
        };
        let deadline = Instant::now() + HTTP_POST_DEFAULT_TIMEOUT;
        let stream = connect_first_reachable(&[closed, good], deadline);
        assert!(
            stream.is_ok(),
            "second resolved address must be attempted, got {:?}",
            stream.err()
        );
    }

    /// MUTATION GUARD (total deadline): past the deadline no address is tried, even a live one.
    #[test]
    fn connect_first_reachable_stops_at_the_deadline() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind reachable server");
        let good = listener.local_addr().expect("addr");
        let deadline = Instant::now()
            .checked_sub(Duration::from_millis(5))
            .unwrap();
        assert_eq!(
            connect_first_reachable(&[good], deadline)
                .unwrap_err()
                .message(),
            "connect deadline exceeded"
        );
    }

    /// MUTATION GUARD (codex #4391 r3-2). A server that closes early under a
    /// declared Content-Length must surface a transport error, not a
    /// truncated "success" body.
    #[test]
    fn short_content_length_is_a_transport_error() {
        let response =
            b"HTTP/1.1 200 OK\r\nContent-Length: 20\r\nConnection: close\r\n\r\n{}".to_vec();
        let port = spawn_draining_oneshot_server(response);
        let url = url::Url::parse(&format!("http://127.0.0.1:{port}/api/x")).expect("url");
        let result = loopback_http_post_inner(&url, "{}", Duration::from_secs(5));
        assert!(
            matches!(&result, Err(message) if message.contains("truncated response")),
            "short Content-Length must be rejected, got {result:?}"
        );
    }

    #[test]
    fn build_response_rejects_truncated_content_length() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 20\r\n\r\n{}";
        let header_end = find_subslice(raw, b"\r\n\r\n").expect("header terminator") + 4;
        let result = build_response(raw, header_end, Some(20));
        assert!(
            result.is_err(),
            "truncated Content-Length must be rejected, got {result:?}"
        );
    }

    /// MUTATION GUARD (codex #4391 r3-3). We speak HTTP/1.0 precisely so a
    /// compliant server never chunks; a rogue chunked reply must be rejected
    /// fail-closed instead of returning raw chunk framing as a "body".
    #[test]
    fn chunked_response_is_rejected_explicitly() {
        let response = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\nB\r\n{\"ok\":true}\r\n0\r\n\r\n".to_vec();
        let port = spawn_draining_oneshot_server(response);
        let url = url::Url::parse(&format!("http://127.0.0.1:{port}/api/x")).expect("url");
        let result = loopback_http_post_inner(&url, "{}", Duration::from_secs(5));
        assert!(
            matches!(&result, Err(message) if message.contains("chunked")),
            "chunked reply must be an explicit error, got {result:?}"
        );
    }

    /// MUTATION GUARD: the chunked-rejection contract above only holds
    /// because the request line pins HTTP/1.0 (compliant servers must not
    /// chunk to a 1.0 client). Bumping it back to HTTP/1.1 fails this assert.
    #[test]
    fn request_head_speaks_http_1_0_and_closes() {
        let head = build_request_head("/api/x", "127.0.0.1:8791", 2);
        assert!(
            head.starts_with("POST /api/x HTTP/1.0\r\n"),
            "request line must pin HTTP/1.0: {head:?}"
        );
        assert!(head.contains("Connection: close\r\n"), "head={head:?}");
        assert!(head.contains("Content-Length: 2\r\n"), "head={head:?}");
    }

    /// MUTATION GUARD (fail-closed request-target sanitizer). A request target
    /// carrying CR/LF or a raw space must be rejected so it can never reach
    /// the wire (request-line injection / EINVAL-style malformed request).
    #[test]
    fn sanitize_request_target_rejects_control_chars() {
        assert!(
            sanitize_request_target("/api/x", Some("a\r\nb")).is_err(),
            "CRLF in query must be rejected"
        );
        assert!(
            sanitize_request_target("/api/x", Some("a b")).is_err(),
            "raw space in query must be rejected"
        );
        assert!(
            sanitize_request_target("/api/x\u{0007}", None).is_err(),
            "control char in path must be rejected"
        );
        // A normal url-encoded session-key path (with encoded `/` and `:`) is
        // accepted unchanged.
        assert_eq!(
            sanitize_request_target("/api/sessions/claude%2Fhost%3Asess/idle-recap", None)
                .as_deref(),
            Ok("/api/sessions/claude%2Fhost%3Asess/idle-recap"),
        );
    }

    #[test]
    fn invoke_localhost_post_rejects_non_loopback() {
        let body = invoke_localhost_post("https://example.com/api", "{}");
        assert_eq!(body, r#"{"error":"only localhost allowed"}"#);
    }

    #[test]
    fn invoke_localhost_post_returns_error_json_when_local_target_is_down() {
        // Reserve an ephemeral port and immediately drop the listener so
        // the connect attempt fails with a normal io::Error, exercising
        // the catch_unwind / Err mapping path without flake-prone timing.
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port for probe");
        let port = listener.local_addr().expect("local addr").port();
        drop(listener);
        let url = format!("http://127.0.0.1:{port}/api/sessions/x/idle-recap");
        let body = invoke_localhost_post(&url, "{}");
        assert!(
            body.starts_with(r#"{"error":"#),
            "expected error JSON, got: {body}"
        );
        // The response must remain JSON-parseable even when the underlying
        // ureq error embeds quotes or newlines — escape_for_json prevents
        // the regression observed in #2098 where stderr messages broke
        // downstream JSON.parse() on the JS side.
        let parsed: serde_json::Value = serde_json::from_str(&body)
            .unwrap_or_else(|err| panic!("response is not JSON: {err}: {body}"));
        assert!(parsed.get("error").and_then(|v| v.as_str()).is_some());
    }

    #[test]
    fn describe_panic_payload_handles_str_string_and_unknown() {
        let static_panic = std::panic::catch_unwind(|| panic!("static-str")).unwrap_err();
        assert_eq!(describe_panic_payload(static_panic), "static-str");

        let owned_panic =
            std::panic::catch_unwind(|| panic!("{}", String::from("owned-string"))).unwrap_err();
        assert_eq!(describe_panic_payload(owned_panic), "owned-string");

        struct Opaque;
        let opaque_panic = std::panic::catch_unwind(|| std::panic::panic_any(Opaque)).unwrap_err();
        assert_eq!(describe_panic_payload(opaque_panic), "unknown panic");
    }

    /// #2378: with no deadline armed, the timeout resolver must return the
    /// default so live-engine `agentdesk.http.post` calls (which run
    /// outside any bounded eval) retain their full 5s budget.
    #[test]
    fn resolve_http_post_timeout_is_default_without_armed_deadline() {
        assert_eq!(resolve_http_post_timeout(), Ok(HTTP_POST_DEFAULT_TIMEOUT));
    }

    /// #2378: when a tight deadline is armed, the resolver must clamp the
    /// ureq timeout down to (at most) the remaining budget so a localhost
    /// POST cannot block the runtime lock longer than the JS eval's
    /// deadline.
    #[test]
    fn resolve_http_post_timeout_clamps_under_tight_deadline() {
        let _scope = crate::utils::bridge_deadline::ScopedBridgeDeadline::new(
            std::time::Duration::from_millis(150),
        );
        let resolved = resolve_http_post_timeout().expect("deadline still in the future");
        assert!(
            resolved <= std::time::Duration::from_millis(150),
            "ureq timeout must shrink to remaining budget, got {resolved:?}"
        );
    }

    /// #2378: after the armed deadline elapses, the resolver must
    /// short-circuit so we don't issue a doomed request that would block
    /// the runtime past the deadline.
    #[tokio::test]
    async fn resolve_http_post_timeout_errors_after_deadline_elapses() {
        let _scope = crate::utils::bridge_deadline::ScopedBridgeDeadline::new(
            std::time::Duration::from_millis(20),
        );
        let observe_at = std::time::Instant::now() + std::time::Duration::from_millis(40);
        crate::test_support::wait::poll_within(
            async || {
                (std::time::Instant::now() >= observe_at)
                    .then_some(())
                    .ok_or_else(|| "original observation time has not elapsed".into())
            },
            crate::test_support::wait::WAIT_CAP,
            "original bridge observation time",
        )
        .await
        .expect("original bridge observation time must elapse");
        let resolved = resolve_http_post_timeout();
        assert!(
            matches!(&resolved, Err(msg) if msg.contains("deadline passed")),
            "expected deadline-passed error, got {resolved:?}"
        );
    }

    #[test]
    fn escape_for_json_escapes_quotes_and_control_chars() {
        assert_eq!(escape_for_json("plain"), "plain");
        assert_eq!(escape_for_json("with \"quotes\""), "with \\\"quotes\\\"");
        assert_eq!(escape_for_json("line\nbreak"), "line\\nbreak");
        assert_eq!(escape_for_json("\u{0001}"), "\\u0001");
    }

    const CLUSTER: &str = "/api/cluster/nodes";
    const CANDIDATES: &str = "/api/sessions/idle-cleanup-candidates";
    const SLOW: &str = "/api/sessions/slow/kill-tmux";
    const READY: &str = "/api/sessions/ready/kill-tmux";
    const SERVER_WAIT_CAP: Duration = Duration::from_secs(8);

    #[test]
    fn loopback_read_timeout_has_a_canonical_deadline_error() {
        let silent = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/silent", silent.local_addr().unwrap());
        let _deadline = ScopedBridgeDeadline::new(Duration::from_millis(50));
        let reply: Value = serde_json::from_str(&invoke_localhost_post(&url, "{}")).unwrap();
        assert_eq!(reply, json!({"error": "response deadline exceeded"}));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn quickjs_idle_kill_settles_deadline_and_resumes_after_the_slow_post() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            tokio::time::timeout(SERVER_WAIT_CAP, serve_ticks(listener))
                .await
                .expect("both ticks must reach the loopback server")
        });
        let runtime = Runtime::new().unwrap();
        let ctx = Context::full(&runtime).unwrap();
        let slot = new_eval_deadline_slot();
        install_policy_interrupt_handler(&runtime, slot.clone(), None);

        ctx.with(|ctx| {
        ctx.globals().set("port", port).unwrap();
        ctx.eval::<(), _>(r#"
            var warnings = [], errors = [], infos = [];
            var timeouts = {};
            var rows = ["slow", "ready"].map(function(key, index) {
                return {
                    session_key: key, provider: "claude", instance_id: null,
                    last_seen_at: "2000-01-01T00:00:00Z",
                    idle_cursor_at: "2000-01-01T00:00:00Z", idle_cursor_id: String(index + 1)
                };
            });
            var agentdesk = {
                config: { get: function() { return port; } },
                db: { query: function(sql, params) {
                    if (sql.startsWith("SELECT DISTINCT")) return [{ instance_id: null }];
                    if (sql.indexOf("::bigint") >= 0) return rows.slice(Number(params[params.length - 1]));
                    return rows;
                } },
                log: {
                    warn: function(line) { warnings.push(line); },
                    error: function(line) { errors.push(line); },
                    info: function(line) { infos.push(line); }
                }
            };
            var module = { exports: {} };
        "#).unwrap();
        register_http_ops(&ctx).unwrap();
        ctx.eval::<(), _>(include_str!("../../../policies/timeouts/idle-kill-owners.js")).unwrap();
        ctx.eval::<(), _>(r#"
            var createOwnerGuard = module.exports;
            var require = function(name) {
                if (name !== "./idle-kill-owners") throw Error(name);
                return createOwnerGuard;
            };
            module = { exports: {} };
        "#).unwrap();
        ctx.eval::<(), _>(include_str!("../../../policies/timeouts/idle-kill.js")).unwrap();
        ctx.eval::<(), _>(r#"
            module.exports(timeouts, {
                loadAgentDirectory: function() { return []; },
                backfillMissingSessionAgentIds: function() {},
                parseSessionThreadId: function() { return null; },
                resolveSessionAgentContext: function() { return {}; }
            });
        "#).unwrap();
    });
        let tick = || {
            ctx.with(|ctx| {
                let _deadline = ArmedDeadline::new(&slot, Duration::from_millis(750));
                ctx.eval::<(), _>("timeouts._section_O();")
                    .expect("tick must return before QuickJS interrupts");
            })
        };
        let logs = |name| ctx.with(|ctx| ctx.eval::<Vec<String>, _>(name).unwrap());

        tick();
        assert_eq!(
            logs("warnings"),
            ["[idle-kill] kill-tmux deadline reached; remaining=1"]
        );
        assert!(logs("errors").is_empty());
        assert!(logs("infos").is_empty());

        tick();
        assert_eq!(logs("warnings").len(), 1);
        assert!(logs("errors").is_empty());
        assert!(
            logs("infos")
                .iter()
                .any(|line| line.contains("Killed idle tmux") && line.ends_with("ready"))
        );
        drop(ctx);
        drop(runtime);
        await_within(server, SERVER_WAIT_CAP).await.unwrap();
    }

    async fn serve_ticks(listener: tokio::net::TcpListener) {
        let mut stalled = None;
        let keys = json!({ "keys": ["slow", "ready"] });
        let remaining = json!({ "keys": ["ready"] });
        let cluster = json!({
            "cluster": { "configured_forward_owner_ids": [], "local_instance_id": null },
            "nodes": [],
        });
        for (method, path, request, reply) in [
            ("GET", CLUSTER, Value::Null, Some(cluster.clone())),
            (
                "POST",
                CANDIDATES,
                keys,
                Some(json!({
                    "decisions": {"slow": {"supported": true}, "ready": {"supported": true}}
                })),
            ),
            ("POST", SLOW, Value::Null, None),
            ("GET", CLUSTER, Value::Null, Some(cluster)),
            (
                "POST",
                CANDIDATES,
                remaining,
                Some(json!({
                    "decisions": {"ready": {"supported": true}}
                })),
            ),
            (
                "POST",
                READY,
                Value::Null,
                Some(json!({
                    "ok": true, "tmux_was_alive": true, "tmux_killed": true,
                })),
            ),
        ] {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = BufReader::new(stream);
            let mut line = String::new();
            stream.read_line(&mut line).await.unwrap();
            assert_eq!(line, format!("{method} {path} HTTP/1.0\r\n"));
            let mut body_len = 0;
            loop {
                line.clear();
                assert_ne!(stream.read_line(&mut line).await.unwrap(), 0);
                if line == "\r\n" {
                    break;
                }
                if let Some(len) = line.strip_prefix("Content-Length: ") {
                    body_len = len.trim().parse().unwrap();
                }
            }
            let mut body = vec![0; body_len];
            stream.read_exact(&mut body).await.unwrap();
            if !request.is_null() {
                assert_eq!(serde_json::from_slice::<Value>(&body).unwrap(), request);
            }
            if let Some(reply) = reply {
                let body = reply.to_string();
                let reply = format!(
                    "HTTP/1.0 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                );
                stream.get_mut().write_all(reply.as_bytes()).await.unwrap();
            } else {
                stalled = Some(stream);
            }
        }
        drop(stalled);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn idle_kill_db_work_deadline_settles_and_resumes_pg() {
        use axum::{
            Json, Router,
            extract::Path,
            routing::{get, post},
        };
        use std::sync::{Arc, Mutex};

        use crate::db::auto_queue::test_support::TestPostgresDb;

        let database = TestPostgresDb::create().await;
        let pool = database.connect_and_migrate_with_max_connections(4).await;
        let requests = Arc::new(Mutex::new(Vec::new()));
        let capture_requests = requests.clone();
        let router = Router::new()
        .route(
            CLUSTER,
            get(|| async {
                Json(json!({
                    "cluster": { "configured_forward_owner_ids": [], "local_instance_id": null },
                    "nodes": [],
                }))
            }),
        )
        .route(
            CANDIDATES,
            post(|Json(body): Json<Value>| async move {
                let decisions: serde_json::Map<_, _> = body["keys"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|key| (key.as_str().unwrap().to_owned(), json!({"supported": true})))
                    .collect();
                Json(json!({ "decisions": decisions }))
            }),
        )
        .route(
            "/api/sessions/{key}/kill-tmux",
            post(move |Path(key): Path<String>| {
                let requests = capture_requests.clone();
                async move {
                    requests.lock().unwrap().push(key);
                    Json(json!({"ok": true, "tmux_was_alive": true, "tmux_killed": true}))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (stop, shutdown) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async move {
                    let _ = shutdown.await;
                })
                .await
                .unwrap();
        });
        let policy_pool = pool.clone();
        let checks = tokio::spawn(async move {
            for phase in ["agent", "owner", "page", "aux"] {
                requests.lock().unwrap().clear();
                let runtime = Runtime::new().unwrap();
                let ctx = Context::full(&runtime).unwrap();
                let slot = new_eval_deadline_slot();
                install_policy_interrupt_handler(&runtime, slot.clone(), None);
                ctx.with(|ctx| {
                    ctx.globals().set("port", port).unwrap();
                    ctx.globals().set("phase", phase).unwrap();
                    load_pg_idle_policy(&ctx, policy_pool.clone());
                    let tick = || {
                        let _deadline = ArmedDeadline::new(&slot, Duration::from_millis(900));
                        ctx.eval::<(), _>("timeouts._section_O();")
                            .unwrap_or_else(|error| {
                                panic!("{phase}: QuickJS tick failed: {error}")
                            });
                    };

                    tick();
                    assert!(ctx.eval::<bool, _>("delayed").unwrap(), "{phase}");
                    assert_eq!(
                        ctx.eval::<Vec<String>, _>("warnings").unwrap(),
                        ["[idle-kill] kill-tmux deadline reached; remaining=unknown"],
                        "{phase}",
                    );
                    assert_eq!(ctx.eval::<usize, _>("errors.length").unwrap(), 0, "{phase}");
                    let before: &[&str] = if phase == "aux" { &["slow"] } else { &[] };
                    assert_eq!(requests.lock().unwrap().as_slice(), before, "{phase}");

                    tick();
                    assert_eq!(
                        ctx.eval::<usize, _>("warnings.length").unwrap(),
                        1,
                        "{phase}"
                    );
                    assert_eq!(ctx.eval::<usize, _>("errors.length").unwrap(), 0, "{phase}");
                    assert_eq!(
                        requests.lock().unwrap().as_slice(),
                        ["slow", "ready"],
                        "{phase}"
                    );
                });
            }
        });
        let checked = await_within(checks, Duration::from_secs(20)).await;
        let _ = stop.send(());
        await_within(server, SERVER_WAIT_CAP).await.unwrap();
        await_within(pool.close(), SERVER_WAIT_CAP).await;
        await_within(database.drop(), SERVER_WAIT_CAP).await;
        checked.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tick5min_reserves_idle_settlement_during_slow_g_pg() {
        use crate::db::auto_queue::test_support::TestPostgresDb;
        use crate::engine::{hooks::Hook, loader::load_single_policy};
        use std::path::Path;

        let database = TestPostgresDb::create().await;
        let pool = database.connect_and_migrate_with_max_connections(2).await;
        let policy_pool = pool.clone();
        let checks = tokio::spawn(async move {
            let runtime = Runtime::new().unwrap();
            let ctx = Context::full(&runtime).unwrap();
            let slot = new_eval_deadline_slot();
            install_policy_interrupt_handler(&runtime, slot.clone(), None);
            ctx.with(|ctx| {
            ctx.eval::<(), _>(r#"
                var warnings = [], gQueries = 0, slowG = true, ownerGets = 0, killPosts = 0;
                var resumedSections = 0;
                var agentdesk = {
                    log: {warn: function(line) { warnings.push(line); }, debug: function() {}},
                    config: {get: function(key) { return key === "server_port" ? 8791 : 45; }},
                    pipeline: {
                        getConfig: function() { return {}; },
                        kickoffState: function() { return "requested"; },
                        nextGatedTarget: function() { return "in_progress"; }
                    },
                    http: {
                        get: function() {
                            ownerGets++;
                            return {cluster: {local_instance_id: null, configured_forward_owner_ids: []}, nodes: []};
                        },
                        post: function(url) {
                            if (url.indexOf("idle-cleanup-candidates") >= 0) {
                                return {decisions: {slow: {supported: true}}};
                            }
                            killPosts++;
                            return {ok: true, skipped_provider_activity_guard: true};
                        }
                    }
                };
            "#).unwrap();
            super::super::db_ops::register_db_ops(&ctx, Some(policy_pool.clone())).unwrap();
            ctx.eval::<(), _>(r#"
                var withWorkDeadline = agentdesk.db.withWorkDeadline;
                agentdesk.db.withWorkDeadline = function(reserve, work) {
                    var cutoffAfterO = !slowG && ++resumedSections === 4;
                    return withWorkDeadline(reserve, work) || cutoffAfterO;
                };
                var query = agentdesk.db.query;
                agentdesk.db.query = function(sql, params, reserve) {
                    if (sql.indexOf("FROM task_dispatches WHERE status IN") >= 0) {
                        gQueries++;
                        if (slowG) return query("SELECT 1 FROM pg_sleep(1.5)", [], reserve);
                    }
                    if (sql.indexOf("idle_cursor_at") >= 0) {
                        return [{
                            session_key: "slow", provider: "claude", instance_id: null,
                            idle_cursor_at: "2000-01-01T00:00:00Z", idle_cursor_id: "1"
                        }];
                    }
                    return [];
                };
            "#).unwrap();
        });
            let file = Path::new(env!("CARGO_MANIFEST_DIR")).join("policies/timeouts.js");
            let policy = load_single_policy(&ctx, &file).unwrap();
            ctx.with(|ctx| {
                let tick = |budget| {
                    let _armed = ArmedDeadline::new(&slot, budget);
                    let function = policy.hooks[&Hook::OnTick5min]
                        .clone()
                        .restore(&ctx)
                        .unwrap();
                    function
                        .call::<_, ()>(())
                        .expect("onTick5min must return without a QuickJS interrupt");
                    assert!(crate::utils::bridge_deadline::current_work_deadline().is_none());
                };

                tick(Duration::from_millis(900));
                assert_eq!(ctx.eval::<i32, _>("gQueries").unwrap(), 1);
                assert_eq!(ctx.eval::<i32, _>("ownerGets + killPosts").unwrap(), 0);
                assert_eq!(
                    ctx.eval::<Vec<String>, _>("warnings").unwrap(),
                    ["[idle-kill] kill-tmux deadline reached; remaining=unknown"],
                );

                ctx.eval::<(), _>("slowG = false;").unwrap();
                tick(Duration::from_secs(5));
                assert_eq!(ctx.eval::<i32, _>("gQueries").unwrap(), 2);
                assert_eq!(ctx.eval::<i32, _>("resumedSections").unwrap(), 4);
                assert_eq!(ctx.eval::<i32, _>("killPosts").unwrap(), 1);
                assert_eq!(ctx.eval::<i32, _>("warnings.length").unwrap(), 1);
            });
        });
        let checked = await_within(checks, Duration::from_secs(20)).await;
        await_within(pool.close(), SERVER_WAIT_CAP).await;
        await_within(database.drop(), SERVER_WAIT_CAP).await;
        checked.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tick5min_excludes_hosted_records_in_candidate_sql_pg() {
        use axum::{
            Json, Router,
            extract::Path,
            routing::{get, post},
        };
        use std::sync::{Arc, Mutex};

        use crate::db::auto_queue::test_support::TestPostgresDb;
        use crate::db::dispatched_sessions::hosted_execution::{
            HostedState,
            test_support::{TOKEN, owner, record, wire},
        };
        use crate::engine::{hooks::Hook, loader::load_single_policy};

        const LEGACY: &str = "fixture:AgentDesk-claude-legacy";
        let database = TestPostgresDb::create().await;
        let pool = database.connect_and_migrate_with_max_connections(4).await;
        let hosted = (1..=192)
            .map(|index| {
                let identity = owner(&(800000 + index).to_string());
                wire(&record(
                    &identity,
                    &format!("hosted-{index}"),
                    HostedState::Bound,
                ))
            })
            .collect();
        let inserted = sqlx::query(
            "INSERT INTO sessions (session_key, provider, status, last_heartbeat, identity_kind,
                              discord_token_hash, channel_id, instance_id, hosted_execution)
         SELECT 'fixture:AgentDesk-claude-hosted-' || n, 'claude', 'idle',
                NOW() - INTERVAL '8 hours', 'discord_channel', $1,
                (800000 + n)::text, 'test-node', execution
         FROM jsonb_array_elements($2::jsonb) WITH ORDINALITY AS hosted(execution, n)",
        )
        .bind(TOKEN)
        .bind(Value::Array(hosted))
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(inserted.rows_affected(), 192);
        sqlx::query(
            "INSERT INTO sessions (session_key, provider, status, last_heartbeat, identity_kind,
                              discord_token_hash, channel_id, instance_id)
         VALUES ($1, 'claude', 'idle', NOW() - INTERVAL '7 hours',
                 'discord_channel', $2, '801000', 'fixture-local')",
        )
        .bind(LEGACY)
        .bind(TOKEN)
        .execute(&pool)
        .await
        .unwrap();

        let candidates = Arc::new(Mutex::new(Vec::new()));
        let kills = Arc::new(Mutex::new(Vec::new()));
        let captured_candidates = candidates.clone();
        let captured_kills = kills.clone();
        let router = Router::new()
            .route(
                CLUSTER,
                get(|| async {
                    Json(json!({
                        "cluster": {
                            "local_instance_id": "fixture-local",
                            "configured_forward_owner_ids": ["test-node"],
                            "lease_ttl_secs": 60,
                        },
                        "nodes": [{"instance_id": "test-node", "status": "online",
                                   "last_heartbeat_at": chrono::Utc::now().to_rfc3339()}],
                    }))
                }),
            )
            .route(
                CANDIDATES,
                post(move |Json(body): Json<Value>| {
                    let candidates = captured_candidates.clone();
                    async move {
                        let keys = body["keys"].clone();
                        candidates.lock().unwrap().push(keys.clone());
                        if keys != json!([LEGACY]) {
                            return Json(
                                json!({"deadline_exceeded": true, "request_started": true}),
                            );
                        }
                        Json(json!({"decisions": {(LEGACY): {"supported": true}}}))
                    }
                }),
            )
            .route(
                "/api/sessions/{key}/kill-tmux",
                post(move |Path(key): Path<String>| {
                    let kills = captured_kills.clone();
                    async move {
                        kills.lock().unwrap().push(key);
                        Json(json!({"ok": true, "skipped_provider_activity_guard": true}))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (stop, shutdown) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async move {
                    let _ = shutdown.await;
                })
                .await
                .unwrap();
        });
        let policy_pool = pool.clone();
        let checks = tokio::spawn(async move {
            let runtime = Runtime::new().unwrap();
            let context = Context::full(&runtime).unwrap();
            let slot = new_eval_deadline_slot();
            install_policy_interrupt_handler(&runtime, slot.clone(), None);
            context.with(|ctx| {
                ctx.globals().set("port", port).unwrap();
                ctx.eval::<(), _>(
                    r#"
                var warnings = [];
                var agentdesk = {
                    log: {warn: function(line) { warnings.push(line); },
                          debug: function() {}, info: function() {}},
                    config: {get: function(key) { return key === "server_port" ? port : 45; }},
                    pipeline: {
                        getConfig: function() { return {}; },
                        kickoffState: function() { return "requested"; },
                        nextGatedTarget: function() { return "in_progress"; }
                    }
                };
            "#,
                )
                .unwrap();
                super::super::db_ops::register_db_ops(&ctx, Some(policy_pool)).unwrap();
                register_http_ops(&ctx).unwrap();
            });
            let file =
                std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("policies/timeouts.js");
            let policy = load_single_policy(&context, &file).unwrap();
            context.with(|ctx| {
                let _deadline = ArmedDeadline::new(&slot, Duration::from_secs(5));
                policy.hooks[&Hook::OnTick5min]
                    .clone()
                    .restore(&ctx)
                    .unwrap()
                    .call::<_, ()>(())
                    .unwrap();
                assert!(ctx.eval::<Vec<String>, _>("warnings").unwrap().is_empty());
            });
        });
        let result = await_within(checks, SERVER_WAIT_CAP).await;
        let _ = stop.send(());
        await_within(server, SERVER_WAIT_CAP).await.unwrap();
        await_within(pool.close(), SERVER_WAIT_CAP).await;
        await_within(database.drop(), SERVER_WAIT_CAP).await;
        assert_eq!(candidates.lock().unwrap().as_slice(), [json!([LEGACY])]);
        assert_eq!(kills.lock().unwrap().as_slice(), [LEGACY]);
        result.unwrap();
    }

    fn load_pg_idle_policy(ctx: &rquickjs::Ctx<'_>, pool: sqlx::PgPool) {
        ctx.eval::<(), _>(
            r#"
        var warnings = [], errors = [];
        var timeouts = {}, delayed = false;
        var agentdesk = {
            config: { get: function() { return port; } },
            log: {
                warn: function(line) { warnings.push(line); },
                error: function(line) { errors.push(line); },
                debug: function() {}, info: function() {}
            }
        };
        var module = { exports: {} };
    "#,
        )
        .unwrap();
        register_http_ops(ctx).unwrap();
        super::super::db_ops::register_db_ops(ctx, Some(pool)).unwrap();
        ctx.eval::<(), _>(include_str!(
            "../../../policies/timeouts/idle-kill-owners.js"
        ))
        .unwrap();
        ctx.eval::<(), _>(r#"
        var createOwnerGuard = module.exports;
        var require = function(name) {
            if (name !== "./idle-kill-owners") throw Error(name);
            return createOwnerGuard;
        };
        var query = agentdesk.db.query;
        agentdesk.db.query = function(sql, params, reserveMs) {
            var stage = sql.indexOf("FROM agents") >= 0 ? "agent" :
                sql.indexOf("SELECT DISTINCT") === 0 ? "owner" :
                sql.indexOf("idle_cursor_at") >= 0 ? "page" :
                sql.indexOf("FROM task_dispatches") >= 0 ? "aux" : "backfill";
            if (!delayed && phase === stage) {
                delayed = true;
                return query("SELECT 1 AS delayed FROM pg_sleep(1.5)", [], reserveMs);
            }
            if (stage === "page") {
                var after = sql.indexOf("::bigint") >= 0 ? Number(params[params.length - 1]) : 0;
                return query("SELECT * FROM (VALUES " +
                    "('slow', 'claude', '2000-01-01T00:00:00Z', '2000-01-01T00:00:00Z', '1', 'fake-dispatch')," +
                    "('ready', 'claude', '2000-01-01T00:00:00Z', '2000-01-01T00:00:00Z', '2', NULL)" +
                    ") AS fixture(session_key, provider, last_seen_at, idle_cursor_at, idle_cursor_id, active_dispatch_id) " +
                    "WHERE idle_cursor_id::bigint > ? ORDER BY idle_cursor_id", [after], reserveMs);
            }
            if (stage === "owner") return query("SELECT NULL::text AS instance_id", [], reserveMs);
            return query("SELECT 1 WHERE FALSE", [], reserveMs);
        };
        module = { exports: {} };
    "#).unwrap();
        ctx.eval::<(), _>(include_str!("../../../policies/lib/timeouts-helpers.js"))
            .unwrap();
        ctx.eval::<(), _>("var helpers = module.exports; module = { exports: {} };")
            .unwrap();
        ctx.eval::<(), _>(include_str!("../../../policies/timeouts/idle-kill.js"))
            .unwrap();
        ctx.eval::<(), _>("module.exports(timeouts, helpers);")
            .unwrap();
    }
}

pub(super) fn register_http_ops<'js>(ctx: &Ctx<'js>) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;
    let http_obj = Object::new(ctx.clone())?;

    http_obj.set(
        "__get_raw",
        Function::new(ctx.clone(), |url: String| -> String {
            invoke_localhost_get(&url)
        })?,
    )?;

    http_obj.set(
        "__post_raw",
        Function::new(ctx.clone(), |url: String, body_json: String| -> String {
            invoke_localhost_post(&url, &body_json)
        })?,
    )?;

    http_obj.set(
        "__request_before_raw",
        Function::new(
            ctx.clone(),
            |url: String, body: String, reserve_ms: u32, post: bool| {
                invoke_localhost_before(
                    &url,
                    body,
                    Duration::from_millis(u64::from(reserve_ms)),
                    post,
                )
            },
        )?,
    )?;

    ad.set("http", http_obj)?;

    let _: rquickjs::Value = ctx.eval(
        r#"
        (function() {
            agentdesk.http.get = function(url, reserveMs) {
                if (reserveMs > 0) {
                    return JSON.parse(agentdesk.http.__request_before_raw(url, "", reserveMs, false));
                }
                return JSON.parse(agentdesk.http.__get_raw(url));
            };
            agentdesk.http.post = function(url, body, reserveMs) {
                var payload = JSON.stringify(body);
                if (reserveMs > 0) {
                    return JSON.parse(agentdesk.http.__request_before_raw(url, payload, reserveMs, true));
                }
                return JSON.parse(agentdesk.http.__post_raw(url, payload));
            };
        })();
    "#,
    )?;

    Ok(())
}
