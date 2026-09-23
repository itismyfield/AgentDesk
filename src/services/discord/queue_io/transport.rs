use super::*;

#[cfg(test)]
mod tests;

/// Owned HTTP handle plus borrowed runtime credentials. Gateway cache is an
/// optional optimization; workers use the same intake and policy checks over REST.
pub(super) struct QueueTransport<'a> {
    http: Arc<serenity::Http>,
    ctx: Option<&'a serenity::Context>,
    token: &'a str,
}

impl<'a> QueueTransport<'a> {
    pub(super) fn from_runtime(shared: &'a SharedData) -> Option<Self> {
        Some(Self {
            http: shared.serenity_http_or_token_fallback()?,
            ctx: shared.http.cached_serenity_ctx.get(),
            token: shared.http.cached_bot_token.get()?,
        })
    }

    pub(super) fn intake_deps<'b>(&'b self, shared: &'b Arc<SharedData>) -> router::IntakeDeps<'b> {
        router::IntakeDeps {
            http: &self.http,
            cache: self.ctx.map(|ctx| &ctx.cache),
            ctx_for_chained_dispatch: self.ctx,
            shared,
            token: self.token,
        }
    }
}
