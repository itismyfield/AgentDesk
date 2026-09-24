use std::sync::{Arc, Mutex};

pub(super) fn issue(title: &str, body: Option<&str>, labels: &[&str]) -> super::sync::GhIssue {
    super::sync::GhIssue {
        number: 42,
        state: "OPEN".to_string(),
        title: title.to_string(),
        labels: labels
            .iter()
            .map(|name| super::sync::GhLabel {
                name: (*name).to_string(),
            })
            .collect(),
        body: body.map(str::to_string),
        url: None,
        closed_at: None,
        closed_by_pull_requests_references: Vec::new(),
    }
}

#[derive(Clone)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(super) struct LogCapture {
    pub dispatch: tracing::Dispatch,
    output: LogBuffer,
}

impl LogCapture {
    pub fn new() -> Self {
        crate::logging::test_capture::pin_callsite_interest();
        let output = LogBuffer(Arc::new(Mutex::new(Vec::new())));
        let writer = output.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(move || writer.clone())
            .finish();
        Self {
            dispatch: tracing::Dispatch::new(subscriber),
            output,
        }
    }

    pub fn take(&self) -> String {
        String::from_utf8(std::mem::take(&mut *self.output.0.lock().unwrap())).unwrap()
    }

    pub fn assert_levels(&self, message: &str, expected: &[&str]) {
        let logs = self.take();
        let levels: Vec<_> = logs
            .lines()
            .filter(|line| line.contains(message))
            .map(|line| line.split_whitespace().next().unwrap())
            .collect();
        assert_eq!(levels, expected, "{logs}");
    }
}
