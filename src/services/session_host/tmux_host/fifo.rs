use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;

use super::TmuxHost;
use crate::services::session_host::submit::{
    AuthorizedHostTarget, InputSource, SubmitOutcome, SubmitPayload, Target, WrapperFrame,
};
use crate::services::session_host::{HostMutation, HostRefusal, InteractiveSessionHost};

impl TmuxHost {
    pub(crate) fn submit_wrapper_frame(
        &self,
        path: &str,
        frame: WrapperFrame<'_>,
        nonblocking_open: bool,
    ) -> SubmitOutcome {
        self.submit(
            AuthorizedHostTarget {
                target: Target::Wrapper {
                    path,
                    nonblocking_open,
                },
                source: InputSource::LegacyWrapper,
            },
            SubmitPayload::WrapperFrame(frame),
        )
    }
}

pub(super) fn submit(path: &str, frame: WrapperFrame<'_>, nonblocking_open: bool) -> SubmitOutcome {
    let mut options = std::fs::OpenOptions::new();
    options.write(true);
    if nonblocking_open {
        options.custom_flags(libc::O_NONBLOCK);
    }
    let fifo = match options.open(path) {
        Ok(fifo) => fifo,
        Err(error) => {
            let message = if nonblocking_open {
                if error.raw_os_error() == Some(libc::ENXIO) {
                    format!(
                        "Failed to open input FIFO {path}: No such device (no reader attached: {error})"
                    )
                } else {
                    format!("Failed to open input FIFO {path}: {error}")
                }
            } else {
                format!("Failed to open input FIFO: {error}")
            };
            return SubmitOutcome::wrapper(
                HostMutation::Refused(HostRefusal::Precondition(message.clone())),
                Err(message),
            );
        }
    };
    if nonblocking_open {
        let fd = fifo.as_raw_fd();
        // The owned descriptor remains live until the frame has been written and flushed.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags >= 0 {
            unsafe {
                let _ = libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK);
            }
        }
    }
    write_frame(fifo, frame, path, nonblocking_open)
}

fn write_frame(
    mut fifo: impl Write,
    frame: WrapperFrame<'_>,
    path: &str,
    with_path: bool,
) -> SubmitOutcome {
    let result = match frame {
        WrapperFrame::Encoded(bytes) => fifo.write_all(bytes.as_bytes()),
        WrapperFrame::Line(text) => writeln!(fifo, "{text}"),
    }
    .map_err(|error| {
        if with_path {
            format!("Failed to write to input FIFO {path}: {error}")
        } else {
            format!("Failed to write to input FIFO: {error}")
        }
    })
    .and_then(|()| {
        fifo.flush().map_err(|error| {
            if with_path {
                format!("Failed to flush input FIFO {path}: {error}")
            } else {
                format!("Failed to flush input FIFO: {error}")
            }
        })
    });
    let mutation = match &result {
        Ok(()) => HostMutation::Confirmed,
        Err(error) => HostMutation::Indeterminate(error.clone()),
    };
    SubmitOutcome::wrapper(mutation, result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_partial_fifo_write_is_indeterminate_and_is_never_retried() {
        struct Partial<'a>(&'a mut usize, &'a mut Vec<u8>);
        impl Write for Partial<'_> {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                *self.0 += 1;
                self.1.extend_from_slice(&bytes[..bytes.len().min(3)]);
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "partial frame may have reached the reader",
                ))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                panic!("failed write must not flush")
            }
        }
        let mut writes = 0;
        let mut accepted = Vec::new();
        let outcome = write_frame(
            Partial(&mut writes, &mut accepted),
            WrapperFrame::Encoded("frame\n"),
            "unused",
            false,
        );
        assert!(matches!(outcome.mutation, HostMutation::Indeterminate(_)));
        assert!(
            outcome
                .into_wrapper()
                .unwrap_err()
                .contains("Failed to write to input FIFO:")
        );
        assert_eq!(writes, 1);
        assert_eq!(accepted, b"fra");
    }

    #[test]
    fn fifo_open_failure_is_refused_and_retains_the_legacy_error() {
        let dir = tempfile::tempdir().unwrap();
        for nonblocking in [false, true] {
            let path = dir.path().join("missing");
            let outcome = TmuxHost.submit_wrapper_frame(
                path.to_str().unwrap(),
                WrapperFrame::Encoded("frame\n"),
                nonblocking,
            );
            assert!(matches!(outcome.mutation, HostMutation::Refused(_)));
            assert!(
                outcome
                    .into_wrapper()
                    .unwrap_err()
                    .starts_with("Failed to open input FIFO")
            );
        }
    }
}
