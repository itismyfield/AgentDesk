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
