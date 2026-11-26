use std::cell::RefCell;
use std::fs::{self, File};
use std::os::unix::fs::FileExt;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::services::cluster::stream_relay::SourceFileIdentity;
use crate::services::discord::runtime_store::{AtomicWriteContext, durable_publish};
use crate::services::tmux_common::{session_temp_path, spawn_markers, with_tmux_source_authority};

const STORE: &str = "headless-terminal";

#[derive(Deserialize, Serialize)]
struct WrapperTerminalReads {
    generation: i64,
    dev: u64,
    ino: u64,
    reads: Vec<WrapperTerminalRead>,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct WrapperTerminalRead {
    pub(crate) start: u64,
    pub(crate) end: u64,
    pub(crate) nonce: String,
}

fn read_ledger(session: &str) -> Result<Option<WrapperTerminalReads>, String> {
    let path = session_temp_path(session, STORE);
    match fs::read(path) {
        Ok(raw) => match serde_json::from_slice(&raw) {
            Ok(ledger) => Ok(Some(ledger)),
            Err(error) => {
                tracing::warn!(%session, %error, "invalid wrapper terminal evidence");
                Ok(None)
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

pub(crate) fn wrapper_terminal_reads(
    session: &str,
    generation: i64,
    file: SourceFileIdentity,
) -> Result<Option<Vec<WrapperTerminalRead>>, String> {
    Ok(read_ledger(session)?.map(|ledger| {
        if ledger.matches(generation, file) {
            ledger.reads
        } else {
            Vec::new()
        }
    }))
}

impl WrapperTerminalReads {
    fn matches(&self, generation: i64, file: SourceFileIdentity) -> bool {
        generation != 0
            && self.generation == generation
            && file
                == (SourceFileIdentity::Unix {
                    dev: self.dev,
                    ino: self.ino,
                })
    }
}

pub(crate) struct Capture {
    session: String,
    path: String,
    nonce: String,
    generation: i64,
    file: SourceFileIdentity,
    start: u64,
}

thread_local! {
    static CAPTURE: RefCell<Option<Capture>> = const { RefCell::new(None) };
}

impl Capture {
    pub(crate) fn before_input(session: &str, path: &str, start: u64, nonce: &str) -> Option<Self> {
        if nonce.is_empty() {
            return None;
        }
        with_tmux_source_authority(session, |_| {
            let generation = spawn_markers::read_generation_file_mtime_ns(session);
            let input = File::open(path).ok()?;
            let file = SourceFileIdentity::from_open_file(&input);
            if generation == 0
                || file == SourceFileIdentity::Unavailable
                || input.metadata().ok()?.len() != start
            {
                return None;
            }
            if start > 0 {
                let mut last_byte = [0];
                input.read_exact_at(&mut last_byte, start - 1).ok()?;
                if last_byte != [b'\n'] {
                    return None;
                }
            }
            Some(Self {
                session: session.into(),
                path: path.into(),
                nonce: nonce.into(),
                generation,
                file,
                start,
            })
        })
    }

    pub(crate) fn around_read<T>(capture: Option<Self>, read: impl FnOnce() -> T) -> T {
        struct Restore(Option<Capture>);
        impl Drop for Restore {
            fn drop(&mut self) {
                CAPTURE.with(|slot| slot.replace(self.0.take()));
            }
        }
        let _restore = Restore(CAPTURE.with(|slot| slot.replace(capture)));
        read()
    }

    pub(super) fn take_for(path: &str, start: u64) -> Option<Self> {
        CAPTURE
            .with(|slot| slot.take())
            .filter(|capture| capture.path == path && capture.start == start)
    }

    pub(super) fn record_result(self, opened: &File, end: u64) -> Result<(), String> {
        if end <= self.start || SourceFileIdentity::from_open_file(opened) != self.file {
            return Ok(());
        }
        let SourceFileIdentity::Unix { dev, ino } = self.file else {
            return Ok(());
        };
        with_tmux_source_authority(&self.session, |_| {
            if self.generation != spawn_markers::read_generation_file_mtime_ns(&self.session) {
                return Ok(());
            }
            let current = File::open(&self.path).map_err(|error| error.to_string())?;
            if SourceFileIdentity::from_open_file(&current) != self.file
                || current.metadata().map_err(|error| error.to_string())?.len() < end
            {
                return Ok(());
            }
            let mut ledger = read_ledger(&self.session)?
                .filter(|ledger| ledger.matches(self.generation, self.file))
                .unwrap_or_else(|| WrapperTerminalReads {
                    generation: self.generation,
                    dev,
                    ino,
                    reads: Vec::new(),
                });
            if ledger.reads.iter().any(|read| read.nonce == self.nonce) {
                return Ok(());
            }
            ledger.reads.push(WrapperTerminalRead {
                start: self.start,
                end,
                nonce: self.nonce,
            });
            let data = serde_json::to_string(&ledger).map_err(|error| error.to_string())?;
            let path = session_temp_path(&self.session, STORE);
            durable_publish::critical_publish(
                Path::new(&path),
                &data,
                AtomicWriteContext::new(STORE),
            )
        })
    }
}

// Stop-hook and synthetic Done frames do not name a completed result record.
pub(super) fn successful_result(line: &str) -> bool {
    #[derive(Deserialize)]
    struct Event<'a> {
        #[serde(rename = "type")]
        kind: &'a str,
        #[serde(default)]
        is_error: bool,
    }
    serde_json::from_str::<Event<'_>>(line)
        .is_ok_and(|event| event.kind == "result" && !event.is_error)
}
