// Mark the process before libtest starts threads, including children spawned before a root lookup.
#[used]
#[cfg_attr(
    all(unix, not(target_vendor = "apple")),
    unsafe(link_section = ".init_array")
)]
#[cfg_attr(
    target_vendor = "apple",
    unsafe(link_section = "__DATA,__mod_init_func")
)]
#[cfg_attr(windows, unsafe(link_section = ".CRT$XCU"))]
static TEST_PROCESS_INIT: extern "C" fn() = mark_test_process;

extern "C" fn mark_test_process() {
    // SAFETY: the platform constructor runs before main and before libtest creates threads.
    unsafe { std::env::set_var("AGENTDESK_TEST_RUNTIME", "1") };
}
