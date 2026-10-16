//! Dormant input ownership capabilities; selection and supervisor start are not wired.
#[allow(dead_code)]
pub(crate) mod clear;
#[allow(dead_code)]
pub(crate) mod fence;
pub(crate) mod offer;
#[allow(dead_code)]
pub(crate) mod reconcile;
#[allow(dead_code)]
pub(crate) mod supervisor;
pub(in crate::services::discord) use supervisor::intake;
pub(crate) use supervisor::runtime;

/// Fence, clear and supervisor holds; an unused supervisor registry adds nothing to the fence's.
pub(crate) fn health_reasons() -> Vec<String> {
    reasons_with(runtime::registry())
}

fn reasons_with(registry: &supervisor::Registry) -> Vec<String> {
    let mut reasons = fence::health_reasons();
    if registry.used() {
        reasons.extend(clear::health_reasons());
        reasons.extend(registry.health_reasons());
    }
    reasons
}
