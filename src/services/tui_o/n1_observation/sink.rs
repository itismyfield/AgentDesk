//! The sole observation I/O owner; publishers never wait for this thread.
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, sync_channel};

use super::{Event, OBSERVER, Observer};

const QUEUE_CAP: usize = 1024;
const FILE_CAP: u64 = 16 * 1024 * 1024;

pub(crate) fn initialize(log_dir: &Path) {
    let (sender, receiver) = sync_channel(QUEUE_CAP);
    let observer = Arc::new(Observer::new(sender));
    if OBSERVER.set(observer.clone()).is_err() {
        return;
    }
    let dir = log_dir.to_path_buf();
    let worker = observer.clone();
    if std::thread::Builder::new()
        .name("n1-observation".into())
        .spawn(move || {
            let writer = Log::open(&dir);
            match writer {
                Ok(writer) => consume(&worker, receiver, writer),
                Err(_) => {
                    worker.sink_errors.fetch_add(1, Ordering::SeqCst);
                }
            }
        })
        .is_err()
    {
        observer.sink_errors.fetch_add(1, Ordering::SeqCst);
    }
}

fn consume(observer: &Observer, receiver: Receiver<Event>, mut writer: impl Write) {
    for event in receiver {
        let result = serde_json::to_writer(&mut writer, &event)
            .map_err(io::Error::other)
            .and_then(|_| writer.write_all(b"\n"));
        if result.is_err() {
            observer.sink_errors.fetch_add(1, Ordering::SeqCst);
            break;
        }
    }
}

struct Log {
    path: PathBuf,
    file: File,
    bytes: u64,
}

impl Log {
    fn open(dir: &Path) -> io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join("n1-observation.jsonl");
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let bytes = file.metadata()?.len();
        Ok(Self { path, file, bytes })
    }

    fn rotate(&mut self) -> io::Result<()> {
        // Only this dedicated log's three archives are rotated.
        for index in (1..=3).rev() {
            let to = self.path.with_extension(format!("jsonl.{index}"));
            match std::fs::remove_file(&to) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            let from = if index == 1 {
                self.path.clone()
            } else {
                self.path.with_extension(format!("jsonl.{}", index - 1))
            };
            match std::fs::rename(from, to) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        self.file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        self.bytes = 0;
        Ok(())
    }
}

impl Write for Log {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let count = self.file.write(bytes)?;
        self.bytes += count as u64;
        if self.bytes >= FILE_CAP && bytes == b"\n" {
            self.rotate()?;
        }
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

#[cfg(test)]
#[path = "sink_tests.rs"]
mod tests;
