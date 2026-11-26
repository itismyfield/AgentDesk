use super::{
    Check, CheckGroup, PathBuf, Value, dcserver, fd_usage_near_limit, fd_usage_percent, json,
    tmux_server_pids,
};

pub(super) const FD_HEADROOM_WARN_PERCENT: u64 = 80;
pub(super) const FD_HEADROOM_WARN_REMAINING: u64 = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FdLimitValue {
    Finite(u64),
    Unlimited,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LaunchdMaxfilesLimit {
    soft: FdLimitValue,
    hard: FdLimitValue,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FdUsageSample {
    process: &'static str,
    pid: u32,
    open_files: u64,
    soft_limit: FdLimitValue,
    limit_source: &'static str,
}

fn parse_fd_limit_value(value: &str) -> Option<FdLimitValue> {
    if value.eq_ignore_ascii_case("unlimited") {
        return Some(FdLimitValue::Unlimited);
    }
    value.parse::<u64>().ok().map(FdLimitValue::Finite)
}

fn parse_launchctl_maxfiles_limit(stdout: &str) -> Option<LaunchdMaxfilesLimit> {
    stdout.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        if parts.next()? != "maxfiles" {
            return None;
        }
        let soft = parse_fd_limit_value(parts.next()?)?;
        let hard = parse_fd_limit_value(parts.next()?)?;
        Some(LaunchdMaxfilesLimit { soft, hard })
    })
}

fn read_launchctl_maxfiles_limit() -> Option<LaunchdMaxfilesLimit> {
    let output = std::process::Command::new("launchctl")
        .args(["limit", "maxfiles"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_launchctl_maxfiles_limit(&String::from_utf8_lossy(&output.stdout))
}

fn launchagent_plist_path(label: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join("Library")
            .join("LaunchAgents")
            .join(format!("{label}.plist")),
    )
}

fn launchagent_plist_nofile_limit(label: &str) -> Option<u64> {
    let plist_path = launchagent_plist_path(label)?;
    let output = std::process::Command::new("/usr/libexec/PlistBuddy")
        .arg("-c")
        .arg("Print :SoftResourceLimits:NumberOfFiles")
        .arg(&plist_path)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u64>()
        .ok()
}
fn lsof_fd_column_counts_against_rlimit(fd: &str) -> bool {
    fd.chars()
        .next()
        .map(|first| first.is_ascii_digit())
        .unwrap_or(false)
}

fn open_file_count_for_pid(pid: u32) -> Option<u64> {
    let output = std::process::Command::new("lsof")
        .args(["-nP", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .skip(1)
            .filter(|line| {
                line.split_whitespace()
                    .nth(3)
                    .is_some_and(lsof_fd_column_counts_against_rlimit)
            })
            .count() as u64,
    )
}

fn fd_limit_display(limit: FdLimitValue) -> String {
    match limit {
        FdLimitValue::Finite(value) => value.to_string(),
        FdLimitValue::Unlimited => "unlimited".to_string(),
        FdLimitValue::Unknown => "unknown".to_string(),
    }
}

fn fd_limit_evidence(limit: FdLimitValue) -> Value {
    match limit {
        FdLimitValue::Finite(value) => json!(value),
        FdLimitValue::Unlimited => json!("unlimited"),
        FdLimitValue::Unknown => json!("unknown"),
    }
}

fn sample_usage_percent(sample: &FdUsageSample) -> Option<u64> {
    match sample.soft_limit {
        FdLimitValue::Finite(limit) => Some(fd_usage_percent(sample.open_files, limit)),
        FdLimitValue::Unlimited | FdLimitValue::Unknown => None,
    }
}

fn sample_is_near_limit(sample: &FdUsageSample) -> bool {
    match sample.soft_limit {
        FdLimitValue::Finite(limit) => fd_usage_near_limit(sample.open_files, limit),
        FdLimitValue::Unlimited | FdLimitValue::Unknown => false,
    }
}

fn fd_sample_detail(sample: &FdUsageSample) -> String {
    let limit = fd_limit_display(sample.soft_limit);
    match sample_usage_percent(sample) {
        Some(percent) => format!(
            "{} pid {} uses {}/{} open files ({}%, source={})",
            sample.process, sample.pid, sample.open_files, limit, percent, sample.limit_source
        ),
        None => format!(
            "{} pid {} uses {} open files (limit={}, source={})",
            sample.process, sample.pid, sample.open_files, limit, sample.limit_source
        ),
    }
}

fn file_descriptor_evidence(
    launchd_limit: Option<LaunchdMaxfilesLimit>,
    launchd_job_loaded: bool,
    plist_nofile_limit: Option<u64>,
    samples: &[FdUsageSample],
) -> Value {
    let sample_evidence: Vec<Value> = samples
        .iter()
        .map(|sample| {
            json!({
                "process": sample.process,
                "pid": sample.pid,
                "open_files": sample.open_files,
                "soft_limit": fd_limit_evidence(sample.soft_limit),
                "limit_source": sample.limit_source,
                "usage_percent": sample_usage_percent(sample),
            })
        })
        .collect();

    json!({
        "launchctl_maxfiles_soft": launchd_limit
            .map(|limit| fd_limit_evidence(limit.soft))
            .unwrap_or_else(|| json!("unknown")),
        "launchctl_maxfiles_hard": launchd_limit
            .map(|limit| fd_limit_evidence(limit.hard))
            .unwrap_or_else(|| json!("unknown")),
        "launchd_job_loaded": launchd_job_loaded,
        "plist_soft_number_of_files": plist_nofile_limit,
        "warn_percent": FD_HEADROOM_WARN_PERCENT,
        "warn_remaining": FD_HEADROOM_WARN_REMAINING,
        "samples": sample_evidence,
    })
}
pub(super) fn check_file_descriptor_headroom() -> Check {
    let label = dcserver::current_dcserver_launchd_label();
    let launchd_limit = read_launchctl_maxfiles_limit();
    let launchd_job_loaded = dcserver::is_launchd_job_loaded(&label);
    let plist_nofile_limit = launchagent_plist_nofile_limit(&label);
    let launchd_soft_limit = launchd_limit
        .map(|limit| limit.soft)
        .unwrap_or(FdLimitValue::Unknown);
    let dcserver_soft_limit = if launchd_job_loaded {
        plist_nofile_limit
            .map(FdLimitValue::Finite)
            .unwrap_or(launchd_soft_limit)
    } else {
        launchd_soft_limit
    };
    // dcserver itself is the launchd job and inherits the per-job plist limit.
    // tmux server PIDs are scoped to current-runtime AgentDesk sessions, not
    // all user tmux daemons, so they can use the same resolved limit/source.
    let resolved_limit_source = if launchd_job_loaded && plist_nofile_limit.is_some() {
        "launchd_plist"
    } else {
        "launchctl_maxfiles"
    };

    let mut samples = Vec::new();
    for pid in dcserver::dcserver_instance_pids() {
        if let Some(open_files) = open_file_count_for_pid(pid) {
            samples.push(FdUsageSample {
                process: "dcserver",
                pid,
                open_files,
                soft_limit: dcserver_soft_limit,
                limit_source: resolved_limit_source,
            });
        }
    }
    for pid in tmux_server_pids() {
        if let Some(open_files) = open_file_count_for_pid(pid) {
            samples.push(FdUsageSample {
                process: "tmux",
                pid,
                open_files,
                soft_limit: dcserver_soft_limit,
                limit_source: resolved_limit_source,
            });
        }
    }

    let evidence = file_descriptor_evidence(
        launchd_limit,
        launchd_job_loaded,
        plist_nofile_limit,
        &samples,
    );

    if launchd_limit.is_none() {
        return Check::warn(
            "file_descriptor_headroom",
            CheckGroup::Core,
            "File Descriptor Headroom",
            "launchctl maxfiles limit unavailable",
            "macOS launchd 한도를 읽을 수 없어 tmux/dcserver FD 사용량을 한도와 비교하지 못했습니다.",
        )
        .with_expected_actual("launchctl limit maxfiles readable", "unavailable")
        .with_evidence(evidence)
        .with_next_steps(vec!["launchctl limit maxfiles".to_string()]);
    }

    let pressured: Vec<&FdUsageSample> = samples
        .iter()
        .filter(|sample| sample_is_near_limit(sample))
        .collect();
    if !pressured.is_empty() {
        let detail = pressured
            .iter()
            .map(|sample| fd_sample_detail(sample))
            .collect::<Vec<_>>()
            .join("; ");
        return Check::warn(
            "file_descriptor_headroom",
            CheckGroup::Core,
            "File Descriptor Headroom",
            detail,
            "If the file descriptor count is close to the soft limit, mkfifo/spawn may fail with EMFILE (os error 24). Increase SoftResourceLimits:NumberOfFiles in the launchd plist and restart dcserver/tmux.",
        )
        .with_expected_actual(
            format!(
                "all sampled processes below {FD_HEADROOM_WARN_PERCENT}% usage and more than {FD_HEADROOM_WARN_REMAINING} FDs remaining"
            ),
            pressured
                .iter()
                .map(|sample| fd_sample_detail(sample))
                .collect::<Vec<_>>()
                .join("; "),
        )
        .with_evidence(evidence)
        .with_next_steps(vec![
            format!(
                "/usr/libexec/PlistBuddy -c 'Print :SoftResourceLimits:NumberOfFiles' ~/Library/LaunchAgents/{label}.plist"
            ),
            "launchctl limit maxfiles".to_string(),
            "reload launchd service or restart the AgentDesk/tmux sessions after raising the limit".to_string(),
        ]);
    }

    let detail = if samples.is_empty() {
        format!(
            "launchctl maxfiles soft={}, no dcserver/tmux process samples",
            fd_limit_display(launchd_soft_limit)
        )
    } else {
        let max_sample = samples
            .iter()
            .max_by_key(|sample| sample_usage_percent(sample).unwrap_or(0))
            .expect("samples is not empty");
        fd_sample_detail(max_sample)
    };

    Check::ok(
        "file_descriptor_headroom",
        CheckGroup::Core,
        "File Descriptor Headroom",
        detail,
    )
    .with_expected_actual("fd usage below warning threshold", "ok")
    .with_evidence(evidence)
}
