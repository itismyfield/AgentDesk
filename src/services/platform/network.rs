//! Listener creation must not lend the API socket to provider subprocesses.
use std::io;
use tokio::net::TcpListener;

pub(crate) async fn bind_tcp_listener(address: &str) -> io::Result<TcpListener> {
    #[cfg(not(windows))]
    {
        TcpListener::bind(address).await
    }
    #[cfg(windows)]
    {
        // Mio's Windows socket() path creates inheritable sockets. Rust's std
        // listener requests WSA_FLAG_NO_HANDLE_INHERIT at creation, closing the
        // race before any concurrently starting provider can inherit the port.
        let mut last_error = None;
        for address in tokio::net::lookup_host(address).await? {
            let result = std::net::TcpListener::bind(address).and_then(|listener| {
                listener.set_nonblocking(true)?;
                TcpListener::from_std(listener)
            });
            match result {
                Ok(listener) => return Ok(listener),
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error.unwrap_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "listener address resolved to no sockets",
            )
        }))
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Foundation::{GetHandleInformation, HANDLE_FLAG_INHERIT};

    #[tokio::test]
    async fn api_listener_is_not_inheritable_by_provider_processes() {
        let listener = bind_tcp_listener("127.0.0.1:0").await.unwrap();
        let mut flags = 0;
        // SAFETY: the listener owns a live socket for the entire query and
        // flags points to writable storage of the required type.
        let ok = unsafe { GetHandleInformation(listener.as_raw_socket() as _, &mut flags) };
        assert_ne!(ok, 0);
        assert_eq!(flags & HANDLE_FLAG_INHERIT, 0);
        assert_ne!(listener.local_addr().unwrap().port(), 0);
    }
}
