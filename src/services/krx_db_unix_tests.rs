//! KRX db test files under `cfg(all(test, unix))` that call KE items; a separate host keeps that
//! single cfg predicate so the declaration key stays the same.

#[path = "../db/turns/receipt_tests.rs"]
mod turns_receipt;
