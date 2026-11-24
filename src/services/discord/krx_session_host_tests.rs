//! KRX session_host tests that call discord-layer items, declared here so they leave the KRX crate.
//! The parent mounts this file ungated, so each `mod` carries the exact cfg of its original
//! declaration; every `#[path]` file stays in place.

#[cfg(test)]
#[path = "../session_host/herdr_gate_tests.rs"]
mod herdr_gate;
// The termination service's Herdr fixture, for the relay tests that drive it end to end.
#[cfg(all(test, unix))]
pub(crate) use herdr_gate::p11 as herdr_termination_rig;
#[cfg(unix)]
#[cfg(all(test, unix))]
#[path = "../session_host/herdr/launch_host_t19_real_tests.rs"]
mod launch_host_t19_real;
#[cfg(unix)]
#[cfg(test)]
#[path = "../session_host/herdr/unplaced_rig_tests.rs"]
mod unplaced_rig;
