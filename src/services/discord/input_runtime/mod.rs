//! Dormant input ownership capabilities; selection and supervisor start are not wired.
#[allow(dead_code)]
pub(crate) mod admission;
#[allow(dead_code)]
pub(crate) mod clear;
#[allow(dead_code)]
pub(crate) mod command;
#[allow(dead_code)]
pub(crate) mod effects;
#[allow(dead_code)]
pub(crate) mod fence;
#[allow(dead_code)]
pub(crate) mod ordering;
#[allow(dead_code)]
pub(crate) mod receipt;
#[allow(dead_code)]
pub(crate) mod reconcile;
#[allow(dead_code)]
pub(crate) mod source;
#[allow(dead_code)]
pub(crate) mod supervisor;

#[cfg(test)]
pub(crate) fn mutant(name: &str) -> bool {
    std::env::var("ADK_TEST_INPUT_G1A_MUTANT").is_ok_and(|value| value == name)
}

/// Fence, clear and supervisor holds; an unused supervisor registry adds nothing to the fence's.
pub(crate) fn health_reasons() -> Vec<String> {
    reasons_with(&supervisor::REGISTRY)
}

fn reasons_with(registry: &supervisor::Registry) -> Vec<String> {
    let mut reasons = fence::health_reasons();
    if registry.used() {
        reasons.extend(clear::health_reasons());
        reasons.extend(registry.health_reasons());
    }
    reasons
}
