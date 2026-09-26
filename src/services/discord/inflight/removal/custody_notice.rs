//! Boot custody notice skeleton.

use crate::services::discord::ProviderKind;
use sqlx::PgPool;
use std::path::Path;

pub(super) fn spawn_boot_custody_notice(_provider: &ProviderKind, _pool: Option<PgPool>) {}

pub(super) async fn enqueue_custody_notices(
    _custody: &Path,
    _provider: &ProviderKind,
    _pool: Option<&PgPool>,
) -> usize {
    0
}

pub(super) fn notices(_custody: &Path, _provider: &ProviderKind) -> Vec<(String, String, String)> {
    Vec::new()
}
