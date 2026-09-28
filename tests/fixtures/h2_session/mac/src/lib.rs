extern crate proc_macro;

// Stable Rust has no immediate expansion-time warning: early lints are emitted after `after_expansion`.
// This rustc-shaped diagnostic is the child's only Cargo-visible output, so forwarding it duplicates JSONL.
const MARKER: &str = r#"{"$message_type":"diagnostic","message":"h2 expansion marker","code":{"code":"h2_expansion_marker","explanation":null},"level":"warning","spans":[],"children":[],"rendered":"warning: h2 expansion marker\n"}"#;

#[proc_macro]
pub fn item(_: proc_macro::TokenStream) -> proc_macro::TokenStream {
    eprintln!("{MARKER}");
    "pub fn from_macro() -> u8 { 2 }".parse().unwrap()
}
