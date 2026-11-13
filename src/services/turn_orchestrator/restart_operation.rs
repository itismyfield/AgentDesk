//! Stable restart identity is independent of the row a request happens to read.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Operation {
    namespace: String,
    channel: u64,
    id: String,
}

impl Operation {
    pub(crate) fn new(provider: &str, token: &str, channel: u64, id: &str) -> Result<Self, String> {
        if provider.is_empty() || channel == 0 || id.is_empty() || id.len() > 256 {
            return Err("invalid restart operation".into());
        }
        Ok(Self {
            namespace: format!("{provider}/{token}"),
            channel,
            id: id.into(),
        })
    }

    pub(crate) fn namespace(&self) -> &str {
        &self.namespace
    }

    pub(crate) fn channel(&self) -> u64 {
        self.channel
    }
}

#[cfg(test)]
#[path = "restart_operation_tests.rs"]
mod tests;
