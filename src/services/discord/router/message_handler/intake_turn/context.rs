use super::*;

pub(in crate::services::discord) use crate::services::discord::queue_io::transport::IntakeDeps;

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
