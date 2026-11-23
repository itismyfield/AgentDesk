//! Test entry points install the runtime ports they reach instead of relying on another test.

/// Installs the Codex row reader, the custody reads and (unix) the strict journal append. Each
/// installer keeps its first value and is handed the same function, so repeating it is harmless.
pub(in crate::services::discord) fn install_runtime_ports_for_tests() {
    crate::services::discord::terminal_delivery_custody::install_dl_ports_for_tests();
    #[cfg(unix)]
    crate::services::discord::install_exact_append();
}
