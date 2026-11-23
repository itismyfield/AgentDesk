//! Binding publications reach the observer wake through a hook the Discord runtime installs.

use super::TuiRuntimeBinding;

static PUBLISHED: std::sync::OnceLock<fn(String, TuiRuntimeBinding)> = std::sync::OnceLock::new();

/// Installs the runtime's wake for a newly published binding, beside its health registry.
pub(crate) fn install(published: fn(String, TuiRuntimeBinding)) {
    let _ = PUBLISHED.set(published);
}

/// Announces a published binding; with no runtime installed there is no observer to wake.
pub(super) fn binding_published(tmux: String, binding: TuiRuntimeBinding) {
    if let Some(published) = PUBLISHED.get() {
        published(tmux, binding);
    }
}
