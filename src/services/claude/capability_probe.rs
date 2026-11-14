//! Asks the launch's own Claude CLI, account, folder and model whether ultracode can turn on,
//! through the `get_settings` control answer the TUI's own toggle reads, sending no prompt.
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;

const UNKNOWN: &str =
    "현재 Claude TUI에서 ultra 사용 가능 여부를 확인하지 못했습니다. 모델은 그대로 유지됩니다.";
const FRAMES: &[u8] =
    br#"{"type":"control_request","request_id":"initialize","request":{"subtype":"initialize"}}
{"type":"control_request","request_id":"get_settings","request":{"subtype":"get_settings"}}
"#;
/// Hooks stay off so the probe starts nothing, and ultracode is asked for so the answer shows
/// whether the request takes effect.
const SETTINGS: &str = r#"{"disableAllHooks":true,"ultracode":true}"#;
const TIMEOUT: Duration = Duration::from_secs(5);
/// One answer serves the suggestions typed for a command and the save that follows them.
const FRESH: Duration = Duration::from_secs(30);

type Launch = (Option<String>, Option<PathBuf>, PathBuf);
static ANSWERS: LazyLock<Mutex<HashMap<Launch, (Instant, bool)>>> = LazyLock::new(Mutex::default);

#[derive(Deserialize)]
struct Applied {
    model: String,
    ultracode: bool,
    #[serde(rename = "ultracodeRequested")]
    requested: bool,
    #[serde(rename = "ultracodeAvailable")]
    available: bool,
}

/// Whether a launch of `model` under `home` in `cwd` can turn ultracode on. A launch folder that
/// is not known, a probe that fails and an answer that does not add up are all unknown.
pub(crate) fn ultracode_available(
    model: Option<&str>,
    home: Option<&Path>,
    cwd: Option<&Path>,
) -> Result<bool, String> {
    let cwd = cwd.filter(|cwd| cwd.is_dir()).ok_or(UNKNOWN)?;
    let model = model.map(str::trim).filter(|model| !model.is_empty());
    let launch = (
        model.map(str::to_string),
        home.map(Path::to_path_buf),
        cwd.to_path_buf(),
    );
    let answers = || {
        ANSWERS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    };
    if let Some((at, available)) = answers().get(&launch).copied()
        && at.elapsed() < FRESH
    {
        return Ok(available);
    }
    let available = probe(model, home, cwd).map_err(|reason| {
        tracing::warn!("[effort] Claude ultracode capability probe failed: {reason}");
        UNKNOWN.to_string()
    })?;
    answers().insert(launch, (Instant::now(), available));
    Ok(available)
}

fn probe(model: Option<&str>, home: Option<&Path>, cwd: &Path) -> Result<bool, String> {
    // A test never reaches an installed CLI and its account: it names the binary it probes.
    #[cfg(test)]
    if std::env::var_os("AGENTDESK_CLAUDE_PATH").is_none() {
        return Err("no test Claude binary".into());
    }
    let (binary, resolution) = super::resolve_claude_binary()?;
    let mut builder =
        crate::services::claude_command::ClaudeCommandBuilder::for_binary(&binary, &resolution);
    let command = builder.command_mut();
    crate::services::process::configure_child_process_group(command);
    command.args(["-p", "--input-format", "stream-json"]);
    command.args(["--output-format", "stream-json", "--verbose"]);
    command.args(["--no-session-persistence", "--settings", SETTINGS]);
    if let Some(model) = model {
        command.args(["--model", model]);
    }
    if let Some(home) = home {
        command.env("CLAUDE_CONFIG_DIR", home);
    }
    command
        .current_dir(cwd)
        .env_remove("CLAUDECODE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = builder
        .into_command()
        .spawn()
        .map_err(|error| format!("spawn: {error}"))?;
    // Only control frames are written, and the closed input ends the process: no prompt is sent.
    let sent = child
        .stdin
        .take()
        .ok_or_else(|| "no stdin".to_string())
        .and_then(|mut stdin| stdin.write_all(FRAMES).map_err(|error| error.to_string()));
    let output =
        crate::services::process::wait_with_output_timeout(child, TIMEOUT, "claude probe")?;
    sent?;
    observed(&output.stdout)
}

/// The availability in the settings answer, taken only when the requested ultracode is in effect
/// exactly where the answer calls it available.
fn observed(raw: &[u8]) -> Result<bool, String> {
    let answer = raw
        .split(|byte| *byte == b'\n')
        .filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
        .find(|frame| {
            frame["type"] == "control_response"
                && frame["response"]["subtype"] == "success"
                && frame["response"]["request_id"] == "get_settings"
        })
        .ok_or_else(|| "no settings answer".to_string())?;
    let applied: Applied =
        serde_json::from_value(answer["response"]["response"]["applied"].clone())
            .map_err(|error| error.to_string())?;
    if applied.model.is_empty() || !applied.requested || applied.ultracode != applied.available {
        return Err("unproven applied capability".into());
    }
    Ok(applied.available)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn answer(requested: bool, available: bool, active: bool) -> Vec<u8> {
        let settings = serde_json::json!({"type":"control_response","response":{
            "subtype":"success","request_id":"get_settings","response":{"applied":{
                "model":"claude-opus-4-8","ultracode":active,
                "ultracodeRequested":requested,"ultracodeAvailable":available}}}});
        format!(
            "{{\"type\":\"control_response\",\"response\":{{\"subtype\":\"success\",\"request_id\":\"initialize\"}}}}\n{settings}\n"
        )
        .into_bytes()
    }

    #[test]
    fn isolated_probe_reports_availability_separately_from_request_and_active() {
        assert_eq!(observed(&answer(true, true, true)), Ok(true));
        assert_eq!(observed(&answer(true, false, false)), Ok(false));
        // An answer whose request did not land, or whose effect disagrees with it, proves nothing.
        for (requested, available, active) in [
            (false, true, false),
            (true, true, false),
            (true, false, true),
        ] {
            assert!(observed(&answer(requested, available, active)).is_err());
        }
        let no_field = br#"{"type":"control_response","response":{"subtype":"success","request_id":"get_settings","response":{"applied":{"model":"claude-opus-4-8","ultracode":false,"ultracodeRequested":true}}}}"#;
        assert!(observed(no_field).is_err());
        let failed = br#"{"type":"control_response","response":{"subtype":"error","request_id":"get_settings"}}"#;
        assert!(observed(failed).is_err());
        assert!(observed(b"").is_err());
    }

    #[test]
    fn an_unknown_launch_folder_or_binary_is_never_guessed_available() {
        let folder = tempfile::tempdir().unwrap();
        for cwd in [None, Some(folder.path().join("gone"))] {
            assert_eq!(
                ultracode_available(Some("claude-opus-4-8"), None, cwd.as_deref()).unwrap_err(),
                UNKNOWN
            );
        }
        let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let unset = crate::config::TestEnvVarGuard::remove_after_shared_test_env_lock;
        let _bin = unset("AGENTDESK_CLAUDE_PATH");
        assert_eq!(
            ultracode_available(None, None, Some(folder.path())).unwrap_err(),
            UNKNOWN
        );
    }

    /// A stand-in CLI that records how it was run and prints the answer file beside it.
    #[cfg(unix)]
    pub(crate) fn stand_in(dir: &Path, answer: &[u8]) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("claude");
        std::fs::write(dir.join("answer"), answer).unwrap();
        let script = "#!/bin/sh\nd=$(dirname \"$0\")\n[ \"$1\" = --version ] && { echo '2.1.325 (Claude Code)'; exit 0; }\n{ pwd; echo \"${CLAUDE_CONFIG_DIR-unset}\"; printf '%s\\n' \"$@\"; } >> \"$d/runs\"\ncat > \"$d/stdin\"\ncat \"$d/answer\"\n";
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn probe_runs_the_launch_binary_account_folder_and_model_without_a_prompt() {
        let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let dir = tempfile::tempdir().unwrap();
        let dir_path = std::fs::canonicalize(dir.path()).unwrap();
        let (home, cwd) = (dir_path.join("home"), dir_path.join("work"));
        std::fs::create_dir_all(&cwd).unwrap();
        let claude = stand_in(&dir_path, &answer(true, true, true));
        let set = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock;
        let _bin = set("AGENTDESK_CLAUDE_PATH", &claude);
        let ask = |model| ultracode_available(Some(model), Some(&home), Some(&cwd));

        assert_eq!(ask("claude-opus-4-8"), Ok(true));
        let runs = std::fs::read_to_string(dir_path.join("runs")).unwrap();
        let expected = format!(
            "{}\n{}\n-p\n--input-format\nstream-json\n--output-format\nstream-json\n--verbose\n--no-session-persistence\n--settings\n{SETTINGS}\n--model\nclaude-opus-4-8\n",
            cwd.display(),
            home.display()
        );
        assert_eq!(runs, expected, "one run, as the launch would run");
        let sent = std::fs::read_to_string(dir_path.join("stdin")).unwrap();
        let frames: Vec<serde_json::Value> = sent
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(frames.len(), 2);
        assert!(
            frames
                .iter()
                .all(|frame| frame["type"] == "control_request")
        );
        assert_eq!(frames[1]["request"]["subtype"], "get_settings");

        // The same launch inside the freshness window is not probed again; another model is.
        std::fs::write(dir_path.join("answer"), answer(true, false, false)).unwrap();
        assert_eq!(ask("claude-opus-4-8"), Ok(true));
        assert_eq!(ask("claude-haiku-4-5"), Ok(false));
        // A failed or inconsistent answer is unknown, and is asked again rather than remembered.
        std::fs::write(dir_path.join("answer"), answer(true, true, false)).unwrap();
        assert_eq!(ask("claude-sonnet-4-6").unwrap_err(), UNKNOWN);
        std::fs::write(dir_path.join("answer"), answer(true, true, true)).unwrap();
        assert_eq!(ask("claude-sonnet-4-6"), Ok(true));
    }
}
