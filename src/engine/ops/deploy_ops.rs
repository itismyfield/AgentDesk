use rquickjs::{Ctx, Function, Object};

type JsResult<T> = rquickjs::Result<T>;

pub(super) fn register_deploy_ops<'js>(ctx: &Ctx<'js>) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;

    ad.set(
        "deploy",
        Function::new(ctx.clone(), || -> String {
            let scripts_dir = resolve_scripts_dir();
            let Some(scripts_dir) = scripts_dir else {
                return serde_json::to_string(&serde_json::json!({
                    "ok": false,
                    "error": "could not resolve scripts directory"
                }))
                .unwrap_or_default();
            };

            let deploy_dev = scripts_dir.join("deploy-dev.sh");
            let promote = scripts_dir.join("promote-release.sh");

            if !deploy_dev.exists() || !promote.exists() {
                return serde_json::to_string(&serde_json::json!({
                    "ok": false,
                    "error": format!(
                        "deploy scripts not found: deploy-dev={} promote={}",
                        deploy_dev.exists(),
                        promote.exists()
                    )
                }))
                .unwrap_or_default();
            }

            tracing::info!("[deploy-gate] starting deploy-dev.sh");
            let dev_result = run_script(&deploy_dev, &[]);
            if !dev_result.success {
                return serde_json::to_string(&serde_json::json!({
                    "ok": false,
                    "stage": "deploy-dev",
                    "error": dev_result.stderr,
                    "stdout": truncate(&dev_result.stdout, 500),
                    "summary": "deploy-dev.sh failed"
                }))
                .unwrap_or_default();
            }
            tracing::info!("[deploy-gate] deploy-dev.sh succeeded");

            tracing::info!("[deploy-gate] starting promote-release.sh --skip-review");
            let promote_result = run_script(&promote, &["--skip-review"]);
            if !promote_result.success {
                return serde_json::to_string(&serde_json::json!({
                    "ok": false,
                    "stage": "promote-release",
                    "error": promote_result.stderr,
                    "stdout": truncate(&promote_result.stdout, 500),
                    "summary": "promote-release.sh failed"
                }))
                .unwrap_or_default();
            }
            tracing::info!("[deploy-gate] promote-release.sh succeeded");

            serde_json::to_string(&serde_json::json!({
                "ok": true,
                "summary": "deploy-dev + promote-release completed successfully"
            }))
            .unwrap_or_default()
        })?,
    )?;

    Ok(())
}

fn resolve_scripts_dir() -> Option<std::path::PathBuf> {
    if let Some(root) = crate::config::runtime_root() {
        let workspace_scripts = root.join("workspaces/agentdesk/scripts");
        if workspace_scripts.exists() {
            return Some(workspace_scripts);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            let scripts = parent.join("../../workspaces/agentdesk/scripts");
            if scripts.exists() {
                return Some(scripts.canonicalize().unwrap_or(scripts));
            }
        }
    }
    None
}

struct ScriptResult {
    success: bool,
    stdout: String,
    stderr: String,
}

fn run_script(path: &std::path::Path, args: &[&str]) -> ScriptResult {
    match std::process::Command::new("bash")
        .arg(path)
        .args(args)
        .output()
    {
        Ok(output) => ScriptResult {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        },
        Err(e) => ScriptResult {
            success: false,
            stdout: String::new(),
            stderr: format!("failed to execute script: {e}"),
        },
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("...{}", &s[s.len() - max..])
    }
}
