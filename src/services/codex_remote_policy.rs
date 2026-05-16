//! Issue #2193 — compile-time prerequisites flag for the Codex remote
//! SSH runtime gate.
//!
//! `providers.codex.remote_ssh_enabled` is an operator-facing assertion
//! that every clause in `docs/codex-remote-ssh-policy.md` is in place.
//! The ADR ships now; the implementation follow-ups (real
//! `services::remote`, `providers.codex.remote_hosts` allow-list,
//! hardened SSH invocation, process-group cancel, integration test) do
//! not. To keep the gate from becoming a persisted "enabled" signal a
//! partial future implementation could silently honor, the bootstrap
//! step hard-fails whenever the operator flips the flag on while
//! `PREREQUISITES_SATISFIED` is still `false`.
//!
//! When the follow-up work lands, this constant flips to `true` in the
//! same PR that wires the implementation. Until then, the gate is
//! effectively immovable.

/// True only when every ADR follow-up listed in
/// `docs/codex-remote-ssh-policy.md` is implemented:
///
/// 1. `services::remote` is a real SSH implementation that honors the
///    ssh-agent + strict known-hosts + `-F none` invocation contract.
/// 2. `providers.codex.remote_hosts` allow-list is wired into config
///    deserialization and routing.
/// 3. `execute_streaming_remote_direct` exists end-to-end with the
///    process-group cancel path described in the ADR.
/// 4. The cancel integration test asserts no remote Codex descendants
///    survive a local cancel or an SSH session drop.
///
/// Flipping this to `true` without the above is a policy violation.
pub const PREREQUISITES_SATISFIED: bool = false;

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: the gate must stay closed in source control until
    /// the ADR follow-ups land. Flipping this constant to `true`
    /// requires landing the implementation in the same change.
    #[test]
    fn prerequisites_remain_unsatisfied() {
        assert!(
            !PREREQUISITES_SATISFIED,
            "Codex remote SSH prerequisites flag flipped on without the ADR \
             follow-ups (services::remote, allow-list, process-group cancel, \
             integration test). See docs/codex-remote-ssh-policy.md (#2193)."
        );
    }
}
