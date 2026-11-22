use super::*;

#[cfg(test)]
mod tests;

/// Discord dependencies shared by leader and worker intake paths.
/// Workers pass no cache or live Context; leaders supply both for chained dispatch.
#[derive(Clone, Copy)]
pub(in crate::services::discord) struct IntakeDeps<'a> {
    pub http: &'a Arc<serenity::http::Http>,
    pub cache: Option<&'a Arc<serenity::cache::Cache>>,
    pub ctx_for_chained_dispatch: Option<&'a serenity::Context>,
    pub shared: &'a Arc<SharedData>,
    pub token: &'a str,
}

/// Owned HTTP handle plus borrowed runtime credentials. Gateway cache is an
/// optional optimization; workers use the same intake and policy checks over REST.
pub(in crate::services::discord) struct QueueTransport<'a> {
    http: Arc<serenity::Http>,
    ctx: Option<&'a serenity::Context>,
    token: &'a str,
}

impl<'a> QueueTransport<'a> {
    pub(in crate::services::discord) fn from_runtime(shared: &'a SharedData) -> Option<Self> {
        Some(Self {
            http: shared.serenity_http_or_token_fallback()?,
            ctx: shared.http.cached_serenity_ctx.get(),
            token: shared.http.cached_bot_token.get()?,
        })
    }

    pub(in crate::services::discord) fn intake_deps<'b>(
        &'b self,
        shared: &'b Arc<SharedData>,
    ) -> IntakeDeps<'b> {
        IntakeDeps {
            http: &self.http,
            cache: self.ctx.map(|ctx| &ctx.cache),
            ctx_for_chained_dispatch: self.ctx,
            shared,
            token: self.token,
        }
    }
}
