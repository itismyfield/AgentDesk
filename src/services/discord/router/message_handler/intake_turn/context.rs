use super::*;

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

pub(super) fn track_inflight_create(
    saved: &mut bool,
    mut create: impl FnMut(
        &mut InflightTurnState,
    )
        -> Result<(), crate::services::discord::inflight::CreateNewInflightError>,
) -> impl FnMut(
    &mut InflightTurnState,
) -> Result<(), crate::services::discord::inflight::CreateNewInflightError> {
    move |state| {
        let result = create(state);
        *saved = result.is_ok();
        result
    }
}
