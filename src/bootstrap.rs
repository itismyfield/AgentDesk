use anyhow::{Context, Result};
use tracing_subscriber::EnvFilter;

pub(crate) struct BootstrapState {
    pub(crate) config: crate::config::Config,
    pub(crate) db: crate::db::Db,
}

pub(crate) fn initialize() -> Result<BootstrapState> {
    init_tracing()?;

    let runtime_root = crate::config::runtime_root();
    let legacy_scan = runtime_root
        .as_ref()
        .map(|root| crate::services::discord::config_audit::scan_legacy_sources(root))
        .unwrap_or_default();

    if let Some(root) = runtime_root.as_ref() {
        crate::runtime_layout::ensure_runtime_layout(root)
            .map_err(|error| anyhow::anyhow!("Failed to prepare runtime layout: {error}"))?;
    }

    let loaded = if let Some(root) = runtime_root.as_ref() {
        crate::services::discord::config_audit::load_runtime_config(root)
            .map_err(|error| anyhow::anyhow!("Failed to load config after layout prep: {error}"))?
    } else {
        let config = crate::config::load().context("Failed to load config")?;
        crate::services::discord::config_audit::LoadedRuntimeConfig {
            config,
            path: std::path::PathBuf::from("config/agentdesk.yaml"),
            existed: true,
        }
    };

    let db = crate::db::init(&loaded.config).context("Failed to init DB")?;
    crate::services::termination_audit::init_audit_db(db.clone());
    let config = if let Some(root) = runtime_root.as_ref() {
        crate::services::discord::config_audit::audit_and_reconcile(
            root,
            loaded.config,
            loaded.path,
            loaded.existed,
            &db,
            &legacy_scan,
            false,
        )
        .map_err(|error| anyhow::anyhow!("Failed to audit runtime config: {error}"))?
        .config
    } else {
        loaded.config
    };

    Ok(BootstrapState { config, db })
}

fn tracing_env_filter() -> Result<EnvFilter> {
    let directive = "agentdesk=info"
        .parse()
        .map_err(|error| anyhow::anyhow!("Failed to parse tracing directive: {error}"))?;
    Ok(EnvFilter::from_default_env().add_directive(directive))
}

fn build_tracing_subscriber<W>(make_writer: W) -> Result<impl tracing::Subscriber + Send + Sync>
where
    W: for<'writer> tracing_subscriber::fmt::writer::MakeWriter<'writer> + Send + Sync + 'static,
{
    Ok(tracing_subscriber::fmt()
        // launchd/systemd append stdout to dcserver.stdout.log; keep tracing on stdout
        // so watcher/policy-hook/runtime logs land in the file operators inspect first.
        .with_writer(make_writer)
        .with_env_filter(tracing_env_filter()?)
        .finish())
}

fn init_tracing() -> Result<()> {
    let subscriber = build_tracing_subscriber(std::io::stdout)?;
    tracing::subscriber::set_global_default(subscriber)
        .map_err(|error| anyhow::anyhow!("Failed to initialize tracing subscriber: {error}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::build_tracing_subscriber;
    use std::io;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::fmt::writer::MakeWriter;

    #[derive(Clone, Default)]
    struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

    struct SharedBufferGuard(Arc<Mutex<Vec<u8>>>);

    impl io::Write for SharedBufferGuard {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for SharedBuffer {
        type Writer = SharedBufferGuard;

        fn make_writer(&'a self) -> Self::Writer {
            SharedBufferGuard(self.0.clone())
        }
    }

    #[test]
    fn test_build_tracing_subscriber_writes_events_to_configured_writer() {
        let buffer = SharedBuffer::default();
        let subscriber = build_tracing_subscriber(buffer.clone()).unwrap();

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("watcher started");
        });

        let output = String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap();
        assert!(output.contains("watcher started"));
    }
}
