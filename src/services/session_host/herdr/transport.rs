//! Unix-socket transport. Herdr answers one request per connection, so every call, the
//! hello included, dials its own; nothing connects until a caller asks.
#![cfg_attr(not(test), allow(dead_code))]

use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use super::contract::{
    HerdrOutcome, HerdrTransport, HerdrTransportError, ServerHello, ServerWitness, Witnessed,
};
use super::model::{HerdrCall, HerdrEndpoint, HerdrRequest};
use super::observe::{self, RestoreUnverified};
use super::provenance::{self, ProcessStart};
use super::wire::{self, HerdrFraming, LineJsonFraming, MAX_FRAME_BYTES};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HerdrSocketConfig {
    /// Per read or write on the socket.
    pub io_timeout: Duration,
    /// Total budget for one read-only call, retries included.
    pub read_deadline: Duration,
    pub retry_backoff: Duration,
    pub max_frame_bytes: usize,
}

impl Default for HerdrSocketConfig {
    fn default() -> Self {
        Self {
            io_timeout: Duration::from_secs(2),
            read_deadline: Duration::from_secs(5),
            retry_backoff: Duration::from_millis(200),
            max_frame_bytes: MAX_FRAME_BYTES,
        }
    }
}

/// Names the process that accepted a connection: its pid and start.
pub(crate) type PeerReader =
    Box<dyn Fn(&UnixStream) -> Result<(u32, ProcessStart), RestoreUnverified> + Send + Sync>;

/// One dialled connection and the server it reached, read before anything is written.
struct Dialled {
    stream: UnixStream,
    connected_at: SystemTime,
    server: Result<(ServerWitness, SystemTime), RestoreUnverified>,
}

pub(crate) struct HerdrSocketTransport<F: HerdrFraming = LineJsonFraming> {
    socket_path: PathBuf,
    config: HerdrSocketConfig,
    framing: F,
    read_peer: PeerReader,
    /// Held from a mutation's server check to its reply, so mutations go out one at a time.
    mutations: Mutex<()>,
    hellos: AtomicU64,
}

impl<F: HerdrFraming> HerdrSocketTransport<F> {
    /// No I/O: every call dials its own connection.
    pub(crate) fn new(endpoint: &HerdrEndpoint, config: HerdrSocketConfig, framing: F) -> Self {
        Self {
            socket_path: endpoint.socket_path().to_path_buf(),
            config,
            framing,
            read_peer: Box::new(provenance::socket_peer),
            mutations: Mutex::new(()),
            hellos: AtomicU64::new(0),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_peer_reader(self, read_peer: PeerReader) -> Self {
        Self { read_peer, ..self }
    }

    fn dial(&self) -> Result<Dialled, HerdrTransportError> {
        let not_sent = |error: std::io::Error| HerdrTransportError::NotSent(error.to_string());
        let stream = UnixStream::connect(&self.socket_path).map_err(not_sent)?;
        stream
            .set_read_timeout(Some(self.config.io_timeout))
            .map_err(not_sent)?;
        stream
            .set_write_timeout(Some(self.config.io_timeout))
            .map_err(not_sent)?;
        let connected_at = SystemTime::now();
        let server = (self.read_peer)(&stream).map(|(pid, start)| {
            let witness = ServerWitness {
                socket: self.socket_path.clone(),
                pid,
                start: start.identity,
            };
            (witness, start.wall_clock)
        });
        Ok(Dialled {
            stream,
            connected_at,
            server,
        })
    }

    /// Writes one request on `stream` and reads one reply; the stream is not reused.
    fn exchange(&self, stream: UnixStream, call: &HerdrCall) -> HerdrOutcome {
        let frame = self
            .framing
            .encode(call)
            .map_err(HerdrTransportError::NotSent)?;
        let mut writer = &stream;
        if let Err(failure) = wire::write_frame(&mut writer, &frame) {
            let detail = format!(
                "{} of {} bytes: {}",
                failure.written,
                frame.len(),
                failure.error
            );
            return Err(if failure.written == 0 {
                HerdrTransportError::NotSent(detail)
            } else {
                HerdrTransportError::AfterWrite(detail)
            });
        }
        let reply = self
            .framing
            .read_frame(&mut BufReader::new(&stream), self.config.max_frame_bytes)
            .map_err(|error| HerdrTransportError::AfterWrite(error.to_string()))?;
        wire::decode_reply(&reply).map_err(HerdrTransportError::AfterWrite)
    }

    fn attempt(&self, call: &HerdrCall) -> (HerdrOutcome, Witnessed) {
        match self.dial() {
            Ok(dialled) => {
                let witness = dialled.server.map(|(witness, _)| witness);
                (self.exchange(dialled.stream, call), witness)
            }
            Err(error) => (Err(error), Err(RestoreUnverified::NoPeer)),
        }
    }
}

impl<F: HerdrFraming> HerdrTransport for HerdrSocketTransport<F> {
    fn call(&self, call: &HerdrCall) -> (HerdrOutcome, Witnessed) {
        if !call.request.is_read_only() {
            let error = "a mutation needs a verified server witness".to_string();
            return (
                Err(HerdrTransportError::NotSent(error)),
                Err(RestoreUnverified::NoPeer),
            );
        }
        let deadline = Instant::now() + self.config.read_deadline;
        observe::retry_read(deadline, self.config.retry_backoff, || self.attempt(call))
    }

    fn call_with_witness(&self, call: &HerdrCall, expected: &ServerWitness) -> HerdrOutcome {
        if expected.socket != self.socket_path {
            return Err(HerdrTransportError::NotSent(format!(
                "witness for {} on {}",
                expected.socket.display(),
                self.socket_path.display()
            )));
        }
        let _one_at_a_time = self
            .mutations
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let dialled = self.dial()?;
        match &dialled.server {
            Ok((witness, _)) if witness == expected => self.exchange(dialled.stream, call),
            Ok((witness, _)) => Err(HerdrTransportError::NotSent(format!(
                "server changed: {witness:?} is not the verified {expected:?}"
            ))),
            Err(why) => Err(HerdrTransportError::NotSent(format!(
                "server unreadable: {why:?}"
            ))),
        }
    }

    fn hello(&self) -> Result<ServerHello, RestoreUnverified> {
        let dialled = self.dial().map_err(|_| RestoreUnverified::NoPeer)?;
        let (witness, started) = dialled.server?;
        // A process that started after this connection was made is a reused pid: no ping.
        if started > dialled.connected_at {
            return Err(RestoreUnverified::ProcessChanged);
        }
        let ping = HerdrCall {
            id: format!(
                "adk-hello-{}",
                self.hellos.fetch_add(1, Ordering::Relaxed) + 1
            ),
            request: HerdrRequest::Ping {},
        };
        let outcome = self.exchange(dialled.stream, &ping);
        let hello = observe::hello_result(&ping, outcome).map_err(|error| {
            tracing::debug!(?error, "herdr hello refused");
            RestoreUnverified::NoPeer
        })?;
        Ok(ServerHello {
            witness,
            started,
            version: hello.version,
            connected_at: dialled.connected_at,
        })
    }

    fn server_witness(&self) -> Witnessed {
        let dialled = self.dial().map_err(|_| RestoreUnverified::NoPeer)?;
        dialled.server.map(|(witness, _)| witness)
    }
}

#[cfg(test)]
#[path = "transport_tests.rs"]
mod tests;
