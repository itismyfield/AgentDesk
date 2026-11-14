//! Org read ownership. Auth readers receive only owned projections.
mod auth_projection;
mod document;

pub(crate) use auth_projection::{
    AuthCatalogSnapshot, PrimaryProfileMap, ProfileBindingInput, configured_auth_profile,
    provider_primary_profile,
};

// Compatibility wire access is confined to services while Discord consumers migrate.
pub(super) use document::{
    AgentDef, ChannelBinding, OrgSchema, fallback_candidates, load_runtime_document_at,
};

pub(super) use document::parse_org_schema;

pub(crate) fn read_auth_catalog_at(
    path: &std::path::Path,
) -> Result<Option<AuthCatalogSnapshot>, String> {
    document::read_document_at(path).map(|document| document.map(AuthCatalogSnapshot::from))
}

pub(crate) fn load_runtime_auth_catalog() -> Option<AuthCatalogSnapshot> {
    let path =
        crate::config::runtime_root().map(|root| crate::runtime_layout::org_schema_path(&root));
    document::load_runtime_document_at(path.as_deref()).map(AuthCatalogSnapshot::from)
}
