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

    /// The greeting's source message and admission ticket, fixed by the operation alone.
    pub(crate) fn seed_identity(&self) -> (u64, String) {
        use sha2::{Digest, Sha256};
        let mut hash = Sha256::new();
        for part in ["agentdesk-restart-seed-v1", &self.namespace, &self.id] {
            hash.update((part.len() as u64).to_be_bytes());
            hash.update(part.as_bytes());
        }
        hash.update(self.channel.to_be_bytes());
        let digest = hash.finalize();
        let message = u64::from_be_bytes(digest[..8].try_into().expect("eight bytes")) | (1 << 63);
        (message, format!("restart-{}", hex::encode(&digest[..28])))
    }
}

#[cfg(test)]
#[path = "restart_operation_tests.rs"]
mod tests;
