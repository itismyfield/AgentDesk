#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn unreadable_retained_custody_keeps_restored_head_pg() {
    crate::services::discord::commands::control::codex_clear_relaunch_tests::absent_launch_tests::custody_tests::unreadable_retained_custody_case(
        concat!(module_path!(), "::unreadable_retained_custody_keeps_restored_head_pg"),
    ).await;
}
