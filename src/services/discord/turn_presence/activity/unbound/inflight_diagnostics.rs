//! Only fixed labels enter diagnostics; approval dedup never depends on progress deltas.
use super::inflight_identity::Kind;
use super::*;
use std::sync::LazyLock;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::services::discord::turn_presence::activity) enum Rejection {
    ReadFailed {
        phase: &'static str,
        field: &'static str,
    },
    InputsChanged {
        phase: &'static str,
        field: &'static str,
    },
    GuardRejected {
        guard: &'static str,
    },
    Superseded {
        phase: &'static str,
        field: &'static str,
    },
}
impl Rejection {
    pub(super) fn collected(phase: &'static str, error: &str) -> Self {
        let field = match error {
            "owner_epoch" => "owner.epoch",
            "membership" => "owner.membership",
            "providers_busy" => "providers_busy",
            "watch_busy" => "watch_busy",
            "registry_busy" => "registry_busy",
            "core_busy" => "core_busy",
            _ => {
                return Self::ReadFailed {
                    phase,
                    field: "snapshot",
                };
            }
        };
        Self::Superseded { phase, field }
    }
    pub(super) fn retryable(&self) -> bool {
        matches!(
            self,
            Self::InputsChanged {
                phase: "admission_pair",
                ..
            } | Self::Superseded { .. }
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Signature {
    Accepted(Kind),
    Rejected(Rejection),
}
type Slot = (u64, String, u64, &'static str);
static LOGGED: LazyLock<Mutex<BTreeMap<Slot, Signature>>> = LazyLock::new(Default::default);

pub(super) fn record(
    provider: &ProviderKind,
    channel: ChannelId,
    slot: &'static str,
    phase: &'static str,
    result: Result<Kind, Rejection>,
    ignored: &[&'static str],
) -> bool {
    let phase = match &result {
        Err(
            Rejection::ReadFailed { phase, .. }
            | Rejection::InputsChanged { phase, .. }
            | Rejection::Superseded { phase, .. },
        ) => *phase,
        Err(Rejection::GuardRejected { .. }) => "admission_guard",
        Ok(_) => phase,
    };
    let generation = runtime_store::process_generation();
    let signature = match result {
        Ok(kind) => Signature::Accepted(kind),
        Err(error) => Signature::Rejected(error),
    };
    let key = (
        generation,
        provider.as_str().to_owned(),
        channel.get(),
        slot,
    );
    let mut logged = lock(&LOGGED);
    if logged.get(&key) == Some(&signature) {
        return false;
    }
    logged.insert(key, signature.clone());
    drop(logged);
    tracing::info!(boot_generation = generation, channel_id = channel.get(),
        provider = provider.as_str(), slot, phase, result = ?signature,
        ignored_fields = ?ignored, legacy_raw_would_reject = !ignored.is_empty(),
        "[turn_presence] inflight_admission");
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Capture(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            lock(&self.0).extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    #[test]
    fn ten_alternating_progress_ticks_emit_one_approval_and_recovery_emits_again() {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let sink = bytes.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .with_writer(move || Capture(sink.clone()))
            .finish();
        let _subscriber = tracing::subscriber::set_default(subscriber);
        let provider = ProviderKind::Claude;
        let channel = ChannelId::new(77_690_900);
        let mut emitted = 0;
        for tick in 0..10 {
            let ignored: &[&str] = if tick % 2 == 0 {
                &["row.save_generation"]
            } else {
                &[]
            };
            emitted += usize::from(record(
                &provider,
                channel,
                "admission",
                "admission_pair",
                Ok(Kind::Projection),
                ignored,
            ));
        }
        assert_eq!(emitted, 1);
        let output = String::from_utf8(lock(&bytes).clone()).unwrap();
        assert_eq!(
            output.matches("[turn_presence] inflight_admission").count(),
            1
        );
        assert!(output.contains("row.save_generation"));
        let error = Rejection::InputsChanged {
            phase: "admission_pair",
            field: "row.turn_nonce",
        };
        assert!(record(
            &provider,
            channel,
            "admission",
            "admission_pair",
            Err(error.clone()),
            &[]
        ));
        assert!(!record(
            &provider,
            channel,
            "admission",
            "admission_pair",
            Err(error),
            &[]
        ));
        assert!(record(
            &provider,
            channel,
            "admission",
            "admission_pair",
            Ok(Kind::Projection),
            &[]
        ));
        let output = String::from_utf8(lock(&bytes).clone()).unwrap();
        assert_eq!(
            output.matches("[turn_presence] inflight_admission").count(),
            3
        );
        assert!(output.contains("row.turn_nonce") && output.contains("admission_pair"));
    }
}
