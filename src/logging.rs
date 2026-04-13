use anyhow::Result;
use std::sync::OnceLock;
use tracing::field;
use tracing_subscriber::EnvFilter;

fn tracing_env_filter() -> Result<EnvFilter> {
    let directive = "agentdesk=info"
        .parse()
        .map_err(|error| anyhow::anyhow!("Failed to parse tracing directive: {error}"))?;
    Ok(EnvFilter::from_default_env().add_directive(directive))
}

fn init_tracing_once() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_env_filter()?)
        .try_init()
        .map_err(|error| anyhow::anyhow!("Failed to initialize tracing subscriber: {error}"))?;
    Ok(())
}

pub(crate) fn init_tracing() -> Result<()> {
    static TRACING_INIT: OnceLock<std::result::Result<(), String>> = OnceLock::new();

    let init_result =
        TRACING_INIT.get_or_init(|| init_tracing_once().map_err(|error| error.to_string()));
    init_result
        .as_ref()
        .map(|_| ())
        .map_err(|error| anyhow::anyhow!(error.clone()))
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct TraceContext<'a> {
    pub(crate) dispatch_id: Option<&'a str>,
    pub(crate) card_id: Option<&'a str>,
    pub(crate) agent_id: Option<&'a str>,
    pub(crate) hook_name: Option<&'a str>,
}

impl<'a> TraceContext<'a> {
    pub(crate) fn from_payload(payload: &'a serde_json::Value) -> Self {
        Self {
            dispatch_id: find_string(payload, &["dispatch_id", "pending_dispatch_id"]),
            card_id: find_string(payload, &["card_id", "kanban_card_id"]),
            agent_id: find_string(
                payload,
                &[
                    "agent_id",
                    "to_agent_id",
                    "assigned_agent_id",
                    "source_agent",
                ],
            ),
            hook_name: None,
        }
    }

    pub(crate) fn with_dispatch_id(mut self, dispatch_id: Option<&'a str>) -> Self {
        self.dispatch_id = dispatch_id.or(self.dispatch_id);
        self
    }

    pub(crate) fn with_card_id(mut self, card_id: Option<&'a str>) -> Self {
        self.card_id = card_id.or(self.card_id);
        self
    }

    pub(crate) fn with_agent_id(mut self, agent_id: Option<&'a str>) -> Self {
        self.agent_id = agent_id.or(self.agent_id);
        self
    }

    pub(crate) fn with_hook_name(mut self, hook_name: Option<&'a str>) -> Self {
        self.hook_name = hook_name.or(self.hook_name);
        self
    }

    pub(crate) fn span(self, name: &'static str) -> tracing::Span {
        tracing::info_span!(
            "trace_context",
            span_name = name,
            dispatch_id = field::debug(self.dispatch_id),
            card_id = field::debug(self.card_id),
            agent_id = field::debug(self.agent_id),
            hook_name = field::debug(self.hook_name),
        )
    }
}

pub(crate) fn dispatch_span(
    name: &'static str,
    dispatch_id: Option<&str>,
    card_id: Option<&str>,
    agent_id: Option<&str>,
) -> tracing::Span {
    TraceContext::default()
        .with_dispatch_id(dispatch_id)
        .with_card_id(card_id)
        .with_agent_id(agent_id)
        .span(name)
}

pub(crate) fn hook_span(hook_name: &str, payload: &serde_json::Value) -> tracing::Span {
    TraceContext::from_payload(payload)
        .with_hook_name(Some(hook_name))
        .span("policy_hook")
}

fn find_string<'a>(value: &'a serde_json::Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| value.get(key).and_then(|v| v.as_str()))
}

#[cfg(test)]
mod tests {
    use super::tracing_env_filter;
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
    fn tracing_env_filter_writes_agentdesk_info_logs() {
        let buffer = SharedBuffer::default();
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(tracing_env_filter().unwrap())
            .with_writer(buffer.clone())
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "agentdesk::services::discord::tmux", "watcher started");
        });

        let output = String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap();
        assert!(output.contains("watcher started"));
    }
}
