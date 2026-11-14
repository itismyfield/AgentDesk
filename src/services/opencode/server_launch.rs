//! Server launch.

use super::*;

pub(super) fn spawn_server(
    bin: &str,
    resolution: &crate::services::platform::BinaryResolution,
    port: u16,
    password: &str,
    working_dir: &str,
    overlay: &crate::services::provider_auth_profile::ProviderAuthOverlay,
) -> Result<OpenCodeServerProcess, String> {
    let mut cmd = Command::new(bin);
    crate::services::platform::apply_binary_resolution(&mut cmd, resolution);
    configure_child_process_group(&mut cmd);
    crate::services::provider_auth_profile::apply_overlay_to_command(&mut cmd, overlay)?;
    cmd.arg("serve")
        .arg("--hostname")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(port.to_string())
        .env("OPENCODE_SERVER_PASSWORD", password)
        .current_dir(working_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Failed to spawn opencode serve: {e}"))?;
    let startup_output = Arc::new(Mutex::new(OpenCodeStartupOutput::default()));
    if let Some(stdout) = child.stdout.take() {
        drain_startup_output(stdout, startup_output.clone(), StartupStream::Stdout);
    }
    if let Some(stderr) = child.stderr.take() {
        drain_startup_output(stderr, startup_output.clone(), StartupStream::Stderr);
    }
    Ok(OpenCodeServerProcess {
        child,
        startup_output,
    })
}

pub(super) fn drain_startup_output<R>(
    mut reader: R,
    output: Arc<Mutex<OpenCodeStartupOutput>>,
    stream: StartupStream,
) where
    R: Read + Send + 'static,
{
    let _ = thread::spawn(move || {
        let mut buffer = [0_u8; 1024];
        loop {
            let read = match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => read,
                Err(_) => break,
            };
            let chunk = String::from_utf8_lossy(&buffer[..read]);
            let mut output = output.lock().unwrap_or_else(|e| {
                tracing::warn!("Recovered poisoned lock for OpenCodeStartupOutput");
                e.into_inner()
            });
            match stream {
                StartupStream::Stdout => append_bounded(&mut output.stdout, &chunk),
                StartupStream::Stderr => append_bounded(&mut output.stderr, &chunk),
            }
        }
    });
}

#[cfg(all(test, unix))]
mod instance_env_tests {
    use super::*;

    #[test]
    fn test_instance_opencode_direct_command_refuses_before_provider_spawn() {
        crate::services::provider_auth_profile::instance_env_tests::with_instance(
            concat!(
                module_path!(),
                "::test_instance_opencode_direct_command_refuses_before_provider_spawn"
            ),
            || {
                use crate::services::provider_auth_profile::ProviderAuthOverlay;
                let instance = crate::config::test_instance::current().unwrap().unwrap();
                let binary =
                    crate::services::provider_auth_profile::instance_env_tests::fake_provider(
                        &instance.root,
                        "opencode",
                    );
                let resolution = crate::services::platform::BinaryResolution {
                    requested_binary: "opencode".into(),
                    resolved_path: Some(binary.display().to_string()),
                    canonical_path: None,
                    source: None,
                    attempts: vec![],
                    failure_kind: None,
                    exec_path: None,
                };
                let result = spawn_server(
                    binary.to_str().unwrap(),
                    &resolution,
                    1,
                    "synthetic",
                    instance.root.to_str().unwrap(),
                    &ProviderAuthOverlay::default_for(ProviderKind::OpenCode),
                );
                assert!(
                    matches!(result, Err(error) if error == "test instance host declaration rejected")
                );
                assert!(!instance.root.join("opencode.argv").exists());
            },
        );
    }
}
