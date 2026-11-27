#[cfg(any(target_os = "linux", target_os = "windows"))]
use super::dcserver;
use super::{Check, CheckGroup};

#[cfg(target_os = "linux")]
pub(super) fn check_service_manager() -> Check {
    let active = dcserver::is_systemd_service_active();
    let enabled = dcserver::is_systemd_service_enabled();
    if active {
        Check::ok(
            "service_manager",
            CheckGroup::Core,
            "Service Manager",
            "systemd --user — agentdesk-dcserver active",
        )
        .with_expected_actual("systemd user service active", "systemd user service active")
    } else if enabled {
        Check::warn(
            "service_manager",
            CheckGroup::Core,
            "Service Manager",
            "systemd --user — agentdesk-dcserver enabled but inactive",
            "check the status with `systemctl --user status agentdesk-dcserver` or try restarting with `agentdesk doctor --fix`.",
        )
        .with_expected_actual("systemd user service active", "systemd user service enabled but inactive")
        .with_next_steps(vec![
            "systemctl --user status agentdesk-dcserver".to_string(),
            "agentdesk doctor --fix".to_string(),
        ])
    } else {
        Check::warn(
            "service_manager",
            CheckGroup::Core,
            "Service Manager",
            "systemd --user — agentdesk-dcserver not enabled",
            "서비스로 운영할 계획이면 systemd user service 등록 여부를 확인하세요.",
        )
        .with_expected_actual(
            "systemd user service enabled",
            "systemd user service not enabled",
        )
        .with_next_steps(vec![
            "systemctl --user status agentdesk-dcserver".to_string(),
        ])
    }
}

#[cfg(target_os = "windows")]
pub(super) fn check_service_manager() -> Check {
    let installed = dcserver::is_windows_service_installed();
    let running = dcserver::is_windows_service_running();
    if running {
        Check::ok(
            "service_manager",
            CheckGroup::Core,
            "Service Manager",
            "Windows service — AgentDeskDcserver running",
        )
        .with_expected_actual("Windows service running", "Windows service running")
    } else if installed {
        Check::warn(
            "service_manager",
            CheckGroup::Core,
            "Service Manager",
            "Windows service — AgentDeskDcserver installed but not running",
            "check the status with `sc query AgentDeskDcserver` or try restarting with `agentdesk doctor --fix`.",
        )
        .with_expected_actual("Windows service running", "Windows service installed but not running")
        .with_next_steps(vec![
            "sc query AgentDeskDcserver".to_string(),
            "agentdesk doctor --fix".to_string(),
        ])
    } else {
        Check::warn(
            "service_manager",
            CheckGroup::Core,
            "Service Manager",
            "Windows service — AgentDeskDcserver not installed",
            "check whether the deployment is a Windows service or a manual execution.",
        )
        .with_expected_actual("Windows service installed", "Windows service not installed")
        .with_next_steps(vec!["sc query AgentDeskDcserver".to_string()])
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
pub(super) fn check_service_manager() -> Check {
    Check::ok(
        "service_manager",
        CheckGroup::Core,
        "Service Manager",
        "N/A",
    )
}
