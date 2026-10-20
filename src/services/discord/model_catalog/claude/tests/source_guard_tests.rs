//! Refresh-state lock source guard, read from the Claude catalog it pins.

pub(super) fn invariant_claude_model_catalog_global_test_state_uses_canonical_lock_scope() {
    const REFRESH_STATE_STORE: &str = concat!("REFRESH_TASK_RUNNING", ".store");

    let source = include_str!("../../claude.rs");
    let scope = source
        .split("pub(super) fn with_test_refresh_task_state")
        .nth(1)
        .and_then(|suffix| suffix.split("\n}\n").next())
        .expect("test refresh state scope");
    let lock = scope
        .find("acquire_shared_test_env_lock")
        .expect("canonical shared test lock");
    let reset = scope
        .find(REFRESH_STATE_STORE)
        .expect("scoped refresh state reset");
    assert!(lock < reset, "the canonical lock must precede the reset");

    let test_module = source
        .split("#[cfg(test)]\nmod tests")
        .nth(1)
        .expect("Claude catalog test module");
    assert!(
        !test_module.contains(REFRESH_STATE_STORE),
        "tests must mutate refresh state only through the locked scope"
    );
}
