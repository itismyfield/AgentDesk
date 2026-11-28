use std::sync::Arc;

use rquickjs::{Ctx, Result as JsResult};
use sqlx::PgPool;
use tokio::runtime::Handle;

use super::{GLOBAL_RUNTIME_HANDLE, RuntimeRegistryPort};

pub fn register_turn_ops<'js>(ctx: &Ctx<'js>, pool: Option<PgPool>) -> JsResult<()> {
    super::register_turn_ops(ctx, pool)
}

pub fn register_turn_ops_with<'js>(
    ctx: &Ctx<'js>,
    pool: Option<PgPool>,
    registry: impl Fn() -> Option<Arc<dyn RuntimeRegistryPort>> + Send + 'static,
    runtime: impl Fn() -> Option<Handle> + Send + 'static,
) -> JsResult<()> {
    super::register_turn_ops_with(ctx, pool, registry, runtime)
}

pub fn install_runtime_handle(handle: Handle) -> RestoreRuntimeHandle {
    RestoreRuntimeHandle(GLOBAL_RUNTIME_HANDLE.lock().unwrap().replace(handle))
}

pub struct RestoreRuntimeHandle(Option<Handle>);

impl Drop for RestoreRuntimeHandle {
    fn drop(&mut self) {
        *GLOBAL_RUNTIME_HANDLE.lock().unwrap() = self.0.take();
    }
}

#[cfg(test)]
mod registry_tests {
    use rquickjs::Object;
    use std::sync::Arc;

    use crate::engine::ops::turn_ops::test_support::{
        install_runtime_handle, register_turn_ops, register_turn_ops_with,
    };
    use crate::services::discord::health::HealthRegistry;
    use crate::services::discord::health::runtime_registry::{
        RestoreRuntimeRegistry, RuntimeRegistrySlot, publish_runtime_registry,
    };

    #[test]
    fn turn_start_returns_error_without_registry() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _entered = runtime.enter();
        let pool = sqlx::postgres::PgPoolOptions::new()
            .min_connections(0)
            .connect_lazy("postgres://handoff-test@127.0.0.1:1/none")
            .unwrap();
        let slot = Arc::new(RuntimeRegistrySlot::default());
        let lookup_slot = slot.clone();
        let runtime_handle = runtime.handle().clone();
        let js = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&js).unwrap();
        ctx.with(|ctx| {
            ctx.globals()
                .set("agentdesk", Object::new(ctx.clone()).unwrap())
                .unwrap();
            register_turn_ops_with(
                &ctx,
                Some(pool),
                move || lookup_slot.port(),
                move || Some(runtime_handle.clone()),
            )
            .unwrap();
        });
        let assert_unavailable_without_spawn = || {
            let before = runtime.metrics().num_alive_tasks();
            let response = ctx.with(|ctx| {
                ctx.eval::<String, _>(r#"agentdesk.turn.__startRaw("some-agent", "hello", "{}")"#)
                    .unwrap()
            });
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&response).unwrap(),
                serde_json::json!({
                    "ok": false,
                    "status": "unavailable",
                    "error": "Discord not available (standalone mode)"
                })
            );
            assert_eq!(runtime.metrics().num_alive_tasks() - before, 0);
        };
        assert_unavailable_without_spawn();

        let registry = Arc::new(HealthRegistry::new());
        slot.publish(&registry);
        assert!(Arc::ptr_eq(&slot.get().unwrap(), &registry));
        drop(registry);
        assert_unavailable_without_spawn();
    }

    struct HandoffLogWriter(tokio::sync::mpsc::UnboundedSender<String>);

    impl std::io::Write for HandoffLogWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if let Ok(line) = std::str::from_utf8(buf)
                && line.contains("js turn.start failed")
            {
                let _ = self.0.send(line.to_owned());
            }
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn turn_start_dispatches_on_durable_runtime_with_live_registry() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        // A closed pool stops the real registry port at the binding lookup without network I/O.
        let pool = runtime.block_on(async {
            let pool = sqlx::postgres::PgPoolOptions::new()
                .connect_lazy("postgres://handoff-test@127.0.0.1:1/none")
                .unwrap();
            pool.close().await;
            pool
        });
        let registry = Arc::new(HealthRegistry::new());
        let _restore_registry = RestoreRuntimeRegistry::capture();
        publish_runtime_registry(&registry);
        let _restore_runtime = install_runtime_handle(runtime.handle().clone());

        let js = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&js).unwrap();
        let tasks_before_dispatch = runtime.metrics().num_alive_tasks();
        let reply = ctx.with(|ctx| {
            ctx.globals()
                .set("agentdesk", Object::new(ctx.clone()).unwrap())
                .unwrap();
            register_turn_ops(&ctx, Some(pool)).unwrap();
            ctx.eval::<String, _>(
                r#"JSON.stringify(agentdesk.turn.start(" target ", "hello", {channel_kind: " cdx "}))"#,
            )
            .unwrap()
        });
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&reply).unwrap(),
            serde_json::json!({"ok": true, "status": "dispatched", "to_agent_id": "target"})
        );
        assert_eq!(
            runtime.metrics().num_alive_tasks(),
            tasks_before_dispatch + 1
        );

        let (logs, mut received) = tokio::sync::mpsc::unbounded_channel();
        crate::logging::test_capture::pin_callsite_interest();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(move || HandoffLogWriter(logs.clone()))
            .finish();
        let failure = tracing::subscriber::with_default(subscriber, || {
            runtime.block_on(async {
                tokio::time::timeout(std::time::Duration::from_secs(5), received.recv())
                    .await
                    .expect("durable runtime should run the handoff")
                    .unwrap()
            })
        });
        assert!(failure.contains("status=500"), "{failure}");
        assert!(failure.contains("query agent channels:"), "{failure}");
    }
}
