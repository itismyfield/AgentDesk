/// # Safety
/// Call only before the binary starts threads or children.
pub(super) unsafe fn take() -> Option<String> {
    let inherited = std::env::var("GEMINI_API_KEY").ok();
    unsafe { std::env::remove_var("GEMINI_API_KEY") };
    inherited
}
