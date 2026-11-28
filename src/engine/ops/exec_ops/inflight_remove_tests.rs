//! Inflight remove test that arms the DL policy-op unlink hook.
#![cfg(unix)]
use crate::engine::ops::exec_ops::register_exec_ops;
use rquickjs::Object;

struct EnvVarOverride {
    _guard: std::sync::MutexGuard<'static, ()>,
    key: &'static str,
    previous: Option<std::ffi::OsString>,
}

impl EnvVarOverride {
    fn set_path(key: &'static str, value: &std::path::Path) -> Self {
        let guard = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let previous = std::env::var_os(key);
        unsafe { std::env::set_var(key, value) };
        Self {
            _guard: guard,
            key,
            previous,
        }
    }
}

impl Drop for EnvVarOverride {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => unsafe { std::env::set_var(self.key, value) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

fn js_remove(channel: &str) -> serde_json::Value {
    let rt = rquickjs::Runtime::new().expect("runtime");
    let ctx = rquickjs::Context::full(&rt).expect("context");
    ctx.with(|ctx| {
        let agentdesk = Object::new(ctx.clone()).expect("agentdesk object");
        ctx.globals()
            .set("agentdesk", agentdesk)
            .expect("set agentdesk");
        register_exec_ops(&ctx).expect("register exec ops");
        let script = format!(r#"JSON.stringify(agentdesk.inflight.remove("claude", "{channel}"))"#);
        let out: String = ctx.eval(script).expect("remove");
        serde_json::from_str(&out).expect("json")
    })
}

/// The public remove refuses every receipt row shape and a held row, keeping its bytes and
/// lock sidecar; a failed unlink is an Io refusal, and only a real unlink answers ok.
#[cfg(unix)]
#[test]
fn inflight_remove_refuses_receipt_rows_and_never_reports_a_failed_unlink_as_removed() {
    use crate::services::provider::cancel_token_claude_interrupt::HERDR_SETTLEMENT_OVERRIDE;
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::TempDir::new().expect("temp root");
    let _env = EnvVarOverride::set_path("AGENTDESK_ROOT_DIR", temp.path());
    crate::services::discord::install_engine_probes();
    let dir = temp.path().join("runtime/discord_inflight/claude");
    std::fs::create_dir_all(&dir).expect("inflight dir");
    let row = |channel: &str, extra: &str| {
        let path = dir.join(format!("{channel}.json"));
        let body = format!(r#"{{"channel_id":{channel},"user_msg_id":7{extra}}}"#);
        std::fs::write(&path, &body).expect("write row");
        (path, body.into_bytes())
    };
    let refused = |reason: &str| {
        serde_json::json!({"ok": false, "error": match reason {
        "ProtectedReceipt" => "protected receipt",
        _ => "busy",
    }, "reason": reason})
    };
    for settlement in [false, true] {
        HERDR_SETTLEMENT_OVERRIDE.set(settlement);
        for (channel, extra) in [
            (
                "901",
                r#","herdr_before_start_stop":{"protocol":1,"notice":"pending"}"#,
            ),
            ("902", r#","herdr_before_start_stop":{"protocol":9}"#),
            ("903", r#","herdr_before_start_stop":null"#),
            (
                "904",
                r#","herdr_before_start_stop":{"protocol":1},"tail":"#,
            ),
        ] {
            let (path, before) = row(channel, extra);
            assert_eq!(
                js_remove(channel),
                refused("ProtectedReceipt"),
                "{channel} settlement={settlement}"
            );
            assert_eq!(
                std::fs::read(&path).unwrap(),
                before,
                "{channel} bytes kept"
            );
            assert!(
                path.with_extension("json.lock").exists(),
                "{channel} sidecar kept"
            );
        }
    }
    let (held, before) = row("905", "");
    let holder = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(held.with_extension("json.lock"))
        .unwrap();
    holder.lock().unwrap();
    assert_eq!(js_remove("905"), refused("Busy"));
    assert_eq!(std::fs::read(&held).unwrap(), before, "held row kept");
    drop(holder);

    let (path, before) = row("906", "");
    crate::services::discord::policy_op::unlink_hook::install(|path| {
        let parent = path.parent().unwrap();
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o555)).unwrap();
    });
    let failed = js_remove("906");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        crate::services::discord::policy_op::unlink_hook::attempts(),
        1
    );
    crate::services::discord::policy_op::unlink_hook::install(|_| {});
    assert_eq!(failed["ok"], false, "{failed}");
    assert_eq!(failed["reason"], "Io", "{failed}");
    assert!(
        failed["error"].as_str().unwrap().contains("os error"),
        "{failed}"
    );
    assert!(failed.get("removed").is_none(), "{failed}");
    assert_eq!(std::fs::read(&path).unwrap(), before, "unlinked row kept");

    assert_eq!(
        js_remove("906"),
        serde_json::json!({"ok": true, "removed": path.display().to_string()})
    );
    assert_eq!(
        crate::services::discord::policy_op::unlink_hook::attempts(),
        1
    );
    assert!(!path.exists());
    assert_eq!(
        js_remove("906"),
        serde_json::json!({"ok": false, "error": "not found"})
    );
}
