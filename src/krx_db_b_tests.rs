//! KRX db tests that call root-crate items (second db slice), declared here so they leave the KRX crate.
//! Each file stays in place; every `mod` keeps the `cfg(test)` chain of its original declaration.

#[path = "db/auto_queue/run_admission_pg_tests.rs"]
pub(crate) mod run_admission_pg;
