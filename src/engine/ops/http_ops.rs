use rquickjs::{Ctx, Function, Object, Result as JsResult};

// ── HTTP ops ────────────────────────────────────────────────────
//
// agentdesk.http.post(url, body) → response_string
// Synchronous HTTP POST for localhost API calls from policy JS.
// Only allows loopback targets for security.

pub(super) fn register_http_ops<'js>(ctx: &Ctx<'js>) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;
    let http_obj = Object::new(ctx.clone())?;

    http_obj.set(
        "__post_raw",
        Function::new(ctx.clone(), |url: String, body_json: String| -> String {
            let loopback_prefix = format!("http://{}", crate::config::loopback());
            if !url.starts_with(&loopback_prefix) {
                return r#"{"error":"only localhost allowed"}"#.to_string();
            }
            // Run on a dedicated thread to avoid blocking the tokio I/O
            // driver.  ureq is synchronous — if called directly on a tokio
            // worker it can self-deadlock when the target is our own HTTP
            // server (the worker blocks on recv while no other worker is
            // available to handle the incoming request).
            let handle = std::thread::spawn(move || {
                match ureq::AgentBuilder::new()
                    .timeout(std::time::Duration::from_secs(5))
                    .build()
                    .post(&url)
                    .set("Content-Type", "application/json")
                    .send_string(&body_json)
                {
                    Ok(resp) => resp.into_string().unwrap_or_else(|_| "{}".to_string()),
                    Err(e) => format!(r#"{{"error":"{}"}}"#, e),
                }
            });
            handle
                .join()
                .unwrap_or_else(|_| r#"{"error":"thread panic"}"#.to_string())
        })?,
    )?;

    ad.set("http", http_obj)?;

    let _: rquickjs::Value = ctx.eval(
        r#"
        (function() {
            var raw = agentdesk.http.__post_raw;
            agentdesk.http.post = function(url, body) {
                return JSON.parse(raw(url, JSON.stringify(body)));
            };
        })();
    "#,
    )?;

    Ok(())
}
