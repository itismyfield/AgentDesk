//! Store recovery for a hosted channel: a recovered store, a fresh one, or why not.

use std::path::Path;
use tokio::sync::watch;

use super::unless_stopped;
use crate::services::tui_o::writer::resume::Backoff;

use crate::services::tui_o::store::ledger::Unsent;
use crate::services::tui_o::store::{ChannelStore, OStore, StoreConfig, StoreError, transient_io};

pub(super) enum Recovered {
    Store(ChannelStore),
    /// Neither the era nor an `init` names the channel: only a first activation may create it.
    Fresh(OStore),
}

/// Why a store was not recovered; a transient one may recover on a later attempt.
pub(super) struct Unrecovered {
    pub(super) detail: String,
    pub(super) transient: bool,
}

impl Unrecovered {
    pub(super) fn new(detail: String, io: Option<std::io::ErrorKind>) -> Self {
        let transient = io.is_some_and(transient_io);
        Self { detail, transient }
    }

    fn of_store(context: &str, error: &StoreError) -> Self {
        let io = match error {
            StoreError::Io(error) => Some(error.kind()),
            _ => None,
        };
        Self::new(format!("{context}: {error:?}"), io)
    }
}

impl From<&str> for Unrecovered {
    fn from(detail: &str) -> Self {
        Self::new(detail.into(), None)
    }
}

/// Recovers the channel's store, first taking back `unsent` if the ledger still ends with it.
/// Damage, a foreign init, an era channel without init or an init without era holds it.
pub(super) fn recover(
    runtime_root: &Path,
    channel: u64,
    unsent: Option<&Unsent>,
) -> Result<Recovered, Unrecovered> {
    let config = StoreConfig { enabled: true };
    let store = OStore::open_if_enabled(&config, runtime_root)
        .map_err(|error| Unrecovered::new(format!("store: {error}"), Some(error.kind())))?
        .ok_or_else(|| Unrecovered::from("store disabled"))?;
    let era = store
        .read_era()
        .map_err(|error| Unrecovered::of_store("era", &error))?;
    let Some(era) = era else {
        return match store.read_init(channel) {
            Ok(None) => Ok(Recovered::Fresh(store)),
            Ok(Some(_)) => Err("no writer era".into()),
            Err(error) => Err(Unrecovered::of_store("init", &error)),
        };
    };
    let opened = store.open_channel_withdrawing(&era, channel, unsent);
    let opened = opened.map_err(|halt| Unrecovered::new(format!("recovery: {halt:?}"), halt.io))?;
    let Some(opened) = opened else {
        return Ok(Recovered::Fresh(store));
    };
    match opened.init().channel {
        stored if stored != channel => Err(Unrecovered::new(
            format!("store names channel {stored}"),
            None,
        )),
        _ => Ok(Recovered::Store(opened)),
    }
}

/// Retry only passing I/O refusals during boot, until recovery succeeds or hosting stops.
pub(super) async fn recover_boot(
    runtime_root: &Path,
    channel: u64,
    stopping: &watch::Receiver<bool>,
) -> Option<Result<Recovered, Unrecovered>> {
    let mut backoff = Backoff::default();
    loop {
        match recover(runtime_root, channel, None) {
            Err(error) if error.transient => {
                let wait = backoff.next_wait();
                tracing::warn!(
                    channel,
                    detail = error.detail,
                    ?wait,
                    "[tui_o] boot store recovery failed transiently; waiting again"
                );
                unless_stopped(stopping, tokio::time::sleep(wait)).await?;
            }
            result => return Some(result),
        }
    }
}
