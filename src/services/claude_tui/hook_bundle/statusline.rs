use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const WRAPPER: &str = r#"
import hashlib, json, os, pathlib, subprocess, sys, tempfile, time

raw = sys.stdin.buffer.read()
original = None
try:
    config_dir = os.environ.get('CLAUDE_CONFIG_DIR') or str(pathlib.Path.home() / '.claude')
    with open(pathlib.Path(config_dir) / 'settings.json', encoding='utf-8') as stream:
        status = json.load(stream).get('statusLine')
    if isinstance(status, dict) and status.get('type') == 'command':
        command = status.get('command')
        if isinstance(command, str) and command.strip():
            original = command
except Exception:
    pass

temporary = None
try:
    value = json.loads(raw)
    session = value.get('session_id')
    if isinstance(session, str) and session:
        directory = sys.argv[1]
        os.makedirs(directory, mode=0o700, exist_ok=True)
        target = os.path.join(directory, hashlib.sha256(session.encode()).hexdigest() + '.json')
        descriptor, temporary = tempfile.mkstemp(prefix='.statusline-', suffix='.tmp', dir=directory)
        with os.fdopen(descriptor, 'w', encoding='utf-8') as stream:
            json.dump({'session_id': session, 'rate_limits': value.get('rate_limits'),
                       'model': value.get('model'), 'effort': value.get('effort'),
                       'fast_mode': value.get('fast_mode'), 'written_at': time.time()}, stream)
        os.replace(temporary, target)
        temporary = None
except Exception:
    pass
finally:
    if temporary is not None:
        try:
            os.unlink(temporary)
        except OSError:
            pass

if original is not None:
    sys.exit(subprocess.run(original, shell=True, input=raw).returncode)
"#;

pub fn record_dir() -> PathBuf {
    std::env::temp_dir().join("agentdesk-claude-rate-limits")
}

pub fn record_path(root: &Path, session: &str) -> PathBuf {
    root.join(format!("{:x}.json", Sha256::digest(session.as_bytes())))
}

/// Also re-runs on a timer: an idle session emits no update events, and a live settings change
/// is confirmed only by a record written after it.
pub(super) fn settings(root: &Path) -> Value {
    let wrapper = if crate::config::test_instance::environment::claude_login_enabled() {
        WRAPPER.replace(
            "try:\n    config_dir",
            "try:\n    raise RuntimeError(\"isolated status line\")\n    config_dir",
        )
    } else {
        WRAPPER.to_owned()
    };
    json!({
        "type": "command",
        "refreshInterval": 5,
        "command": format!("python3 -c {} {}", super::shell_quote(&wrapper),
            super::shell_quote(&root.to_string_lossy())),
    })
}

pub(super) fn hook_settings(
    config: &super::HookBundleConfig,
    hooks: serde_json::Map<String, Value>,
) -> Value {
    let mut value = json!({"hooks": hooks, "statusLine": settings(&record_dir())});
    if config.provider == "claude"
        && crate::config::test_instance::environment::claude_startup_prompt_accepted(
            &config.endpoint,
        )
    {
        // Only the prepared acknowledgement enters flag settings; user hooks and MCP stay excluded.
        value["skipDangerousModePermissionPrompt"] = json!(true);
    }
    value
}

#[cfg(all(test, unix))]
#[path = "statusline_tests.rs"]
mod tests;
