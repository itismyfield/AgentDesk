//! Discord leaf tests (`codex_absence`, `runtime_store`, `session_transition`) that reach Discord
//! runtime items; each file stays beside the module it tests.

#[path = "../codex_absence/observation_tests.rs"]
mod codex_absence_observation;
#[path = "../codex_absence/replacement_tests.rs"]
mod codex_absence_replacement;
#[path = "../runtime_store/durable_publish_tests.rs"]
mod runtime_store_durable_publish;
#[path = "../session_transition_tests.rs"]
mod session_transition;
