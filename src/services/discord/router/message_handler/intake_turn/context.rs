use super::*;

/// Bundle of Discord-runtime dependencies that `handle_text_message`
/// reads from outside its per-message parameters. Phase 2-pre.2 of
/// intake-node-routing (docs/design/intake-node-routing.md): the body
/// reads only `http` and (optionally) `cache`, both of which are REST-
/// or cache-backed primitives. Worker-side callers without a live shard
/// pass `cache: None` and `ctx_for_chained_dispatch: None`; leader-side
/// callers pass `Some(&ctx.cache)` and `Some(ctx)` to preserve the
/// in-process category cache and the chained-dispatch path.
///
/// `ctx_for_chained_dispatch` is the only remaining `&serenity::Context`
/// dependency: `DiscordGateway::new` accepts an optional
/// `LiveDiscordTurnContext { ctx, .. }` that wires the queued-turn
/// hand-off back through the gateway's live shard. Workers cannot
/// participate in that flow (they have no shard) so they pass `None`
/// and the gateway is constructed with `live_turn = None`.
#[derive(Clone, Copy)]
pub(in crate::services::discord) struct IntakeDeps<'a> {
    pub http: &'a Arc<serenity::http::Http>,
    pub cache: Option<&'a Arc<serenity::cache::Cache>>,
    pub ctx_for_chained_dispatch: Option<&'a serenity::Context>,
    pub shared: &'a Arc<SharedData>,
    pub token: &'a str,
}
