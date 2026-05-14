use rquickjs::{Ctx, Function, Object, Result as JsResult};

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

fn invoke_localhost_post(url: &str, body_json: &str) -> String {
    if !crate::utils::loopback_url::is_loopback_url(url, None) {
        return r#"{"error":"only localhost allowed"}"#.to_string();
    }
    let url_owned = url.to_string();
    let body_owned = body_json.to_string();
    // Run on a dedicated thread to avoid blocking the tokio I/O driver.
    // ureq is synchronous — if called directly on a tokio worker it can
    // self-deadlock when the target is our own HTTP server (the worker
    // blocks on recv while no other worker is available to handle the
    // incoming request).
    let handle = std::thread::spawn(move || {
        // #2098: ureq-2.12.1's response.rs (`failed to read exact buffer
        // length from stream`) can panic on the read path under certain
        // server-side response shapes. Without `catch_unwind` the panic
        // unwinds the worker thread and the caller only gets
        // `{"error":"thread panic"}`, plus a noisy stderr entry every
        // tick. Catch the panic so it becomes a normal JSON error and
        // the policy can decide whether to retry.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let request = ureq::AgentBuilder::new()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .post(&url_owned)
                .set("Content-Type", "application/json");
            request.send_string(&body_owned).map(|resp| {
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
            Ok(Ok(body)) => body,
            Ok(Err(err)) => format!(r#"{{"error":"{}"}}"#, escape_for_json(&err.to_string())),
            Err(payload) => format!(
                r#"{{"error":"ureq panic: {}"}}"#,
                escape_for_json(&describe_panic_payload(payload))
            ),
        }
    });
    handle
        .join()
        .unwrap_or_else(|_| r#"{"error":"thread panic"}"#.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn escape_for_json_escapes_quotes_and_control_chars() {
        assert_eq!(escape_for_json("plain"), "plain");
        assert_eq!(escape_for_json("with \"quotes\""), "with \\\"quotes\\\"");
        assert_eq!(escape_for_json("line\nbreak"), "line\\nbreak");
        assert_eq!(escape_for_json("\u{0001}"), "\\u0001");
    }
}

pub(super) fn register_http_ops<'js>(ctx: &Ctx<'js>) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;
    let http_obj = Object::new(ctx.clone())?;

    http_obj.set(
        "__post_raw",
        Function::new(ctx.clone(), |url: String, body_json: String| -> String {
            invoke_localhost_post(&url, &body_json)
        })?,
    )?;

    ad.set("http", http_obj)?;

    let _: rquickjs::Value = ctx.eval(
        r#"
        (function() {
            agentdesk.http.post = function(url, body) {
                return JSON.parse(agentdesk.http.__post_raw(url, JSON.stringify(body)));
            };
        })();
    "#,
    )?;

    Ok(())
}
