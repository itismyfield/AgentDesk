//! `org_schema` tests that reach the org writer and `tui_o` exact-submission tests that reach the
//! strict journal; each file stays beside the module it tests.

#[path = "../org_schema/auth_selection_tests.rs"]
mod org_schema_auth_selection;
#[path = "../../tui_o/exact_submission_tests.rs"]
mod tui_o_exact_submission;
