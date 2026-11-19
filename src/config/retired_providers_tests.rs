use std::sync::{Arc, Mutex};

use tracing_subscriber::layer::SubscriberExt;

const CASE: &str = "ADK_TEST_RETIRED_PROVIDER_BOOT_CASE";

fn boot_config(provider: &str) -> crate::config::Config {
    serde_json::from_value(serde_json::json!({
        "server": {},
        "cluster": {"instance_id": "test-node"},
        "providers": {provider: {}},
        "agents": [{
            "id": "fixture-agent",
            "name": "fixture",
            "provider": provider,
            "channels": {provider: "1470000000000000001"},
        }],
        "discord": {"bots": {provider: {"provider": provider}}},
    }))
    .unwrap()
}

struct RetiredWarnings(Arc<Mutex<usize>>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for RetiredWarnings {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        let meta = event.metadata();
        if *meta.level() == tracing::Level::WARN
            && meta.target() == module_path!().trim_end_matches("::tests")
        {
            *self.0.lock().unwrap() += 1;
        }
    }
}

/// The boot gate installs process-wide snapshots, so each case runs in its own child process.
#[tokio::test]
async fn retired_provider_config_boots_with_exactly_one_warning() {
    let Ok(provider) = std::env::var(CASE) else {
        let test = concat!(
            module_path!(),
            "::retired_provider_config_boots_with_exactly_one_warning"
        )
        .trim_start_matches("agentdesk::");
        for provider in ["gemini", "claude"] {
            let root = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", test, "--nocapture"])
                .env(CASE, provider)
                .env("AGENTDESK_ROOT_DIR", root.path())
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(stdout.contains("running 1 test"), "{stdout}");
            assert!(
                output.status.success(),
                "provider={provider}\nstdout={stdout}\nstderr={}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        return;
    };
    let warnings = Arc::new(Mutex::new(0));
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::registry().with(RetiredWarnings(warnings.clone())),
    );
    let result =
        crate::bootstrap::prepare_and_install_boot_snapshots(&boot_config(&provider), None).await;
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(*warnings.lock().unwrap(), usize::from(provider == "gemini"));
}
