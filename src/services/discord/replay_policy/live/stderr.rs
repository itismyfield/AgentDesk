//! Cancellation closes the owned pipe and joins its reader even when a descendant keeps writing.

use std::io::{self, Read};
use std::process::ChildStderr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub(crate) struct StderrRead {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<io::Result<String>>>,
}

impl StderrRead {
    pub(super) fn start(mut stderr: ChildStderr) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let cancelled = Arc::clone(&stop);
        let worker = std::thread::Builder::new()
            .name("replay-stderr".into())
            .spawn(move || {
                prepare(&stderr)?;
                let mut bytes = Vec::new();
                let mut buffer = [0; 8192];
                while !cancelled.load(Ordering::Acquire) {
                    match read_available(&mut stderr, &mut buffer) {
                        Ok(0) => return String::from_utf8(bytes).map_err(io::Error::other),
                        Ok(n) => bytes.extend_from_slice(&buffer[..n]),
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                        Err(error) => return Err(error),
                    }
                }
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "stderr observation cancelled",
                ))
            })
            .ok();
        Self { stop, worker }
    }

    pub(super) fn collect(mut self, timeout: Duration) -> io::Result<String> {
        let deadline = Instant::now() + timeout;
        while self
            .worker
            .as_ref()
            .is_some_and(|worker| !worker.is_finished())
        {
            if Instant::now() >= deadline {
                self.stop.store(true, Ordering::Release);
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        self.worker
            .take()
            .ok_or_else(|| io::Error::other("stderr reader missing"))?
            .join()
            .map_err(|_| io::Error::other("stderr reader panicked"))?
    }
}

impl Drop for StderrRead {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(unix)]
fn prepare(stderr: &ChildStderr) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let fd = stderr.as_raw_fd();
    // The descriptor belongs only to this reader and is closed when the worker exits.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(unix))]
fn prepare(_stderr: &ChildStderr) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn read_available(stderr: &mut ChildStderr, buffer: &mut [u8]) -> io::Result<usize> {
    stderr.read(buffer)
}

#[cfg(windows)]
fn read_available(stderr: &mut ChildStderr, buffer: &mut [u8]) -> io::Result<usize> {
    use std::os::windows::io::AsRawHandle;
    let mut available = 0;
    // Peek never waits for EOF or a descendant; read only the bytes it reported ready.
    let ok = unsafe {
        windows_sys::Win32::System::Pipes::PeekNamedPipe(
            stderr.as_raw_handle(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            &mut available,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        let error = io::Error::last_os_error();
        return if error.raw_os_error() == Some(109) {
            Ok(0)
        } else {
            Err(error)
        };
    }
    if available == 0 {
        return Err(io::ErrorKind::WouldBlock.into());
    }
    let len = buffer.len().min(available as usize);
    stderr.read(&mut buffer[..len])
}

#[cfg(not(any(unix, windows)))]
fn read_available(_stderr: &mut ChildStderr, _buffer: &mut [u8]) -> io::Result<usize> {
    Err(io::ErrorKind::Unsupported.into())
}
