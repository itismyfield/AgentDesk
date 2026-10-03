//! Unix-socket transport. Nothing connects until a caller asks; one request
//! is in flight per connection, and any unclear exchange drops the connection.
#![cfg_attr(not(test), allow(dead_code))]

use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use super::contract::{HerdrOutcome, HerdrTransport, HerdrTransportError, ServerPeer};
use super::model::{HerdrCall, HerdrEndpoint, HerdrRequest};
use super::observe::{self, HerdrHello, RestoreUnverified};
use super::wire::{self, HerdrFraming, LineJsonFraming, MAX_FRAME_BYTES};
use crate::services::session_host::model::HostError;

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

struct Connection {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    generation: u64,
    hello: HerdrHello,
    /// Socket peer pid read when the connection opened; `None` if the OS gave none.
    peer_pid: Option<u32>,
    connected_at: SystemTime,
}

impl Connection {
    fn exchange(
        &mut self,
        framing: &dyn HerdrFraming,
        call: &HerdrCall,
        max: usize,
    ) -> HerdrOutcome {
        let frame = framing.encode(call).map_err(HerdrTransportError::NotSent)?;
        if let Err(failure) = wire::write_frame(&mut self.writer, &frame) {
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
        let reply = framing
            .read_frame(&mut self.reader, max)
            .map_err(|error| HerdrTransportError::AfterWrite(error.to_string()))?;
        wire::decode_reply(&reply).map_err(HerdrTransportError::AfterWrite)
    }
}

pub(crate) struct HerdrSocketTransport<F: HerdrFraming = LineJsonFraming> {
    socket_path: PathBuf,
    config: HerdrSocketConfig,
    framing: F,
    connection: Mutex<Option<Connection>>,
    generations: AtomicU64,
}

impl<F: HerdrFraming> HerdrSocketTransport<F> {
    /// No I/O: the socket is opened by `connect` or a read-only call.
    pub(crate) fn new(endpoint: &HerdrEndpoint, config: HerdrSocketConfig, framing: F) -> Self {
        Self {
            socket_path: endpoint.socket_path().to_path_buf(),
            config,
            framing,
            connection: Mutex::new(None),
            generations: AtomicU64::new(0),
        }
    }

    fn slot(&self) -> MutexGuard<'_, Option<Connection>> {
        self.connection
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Replaces any current connection with a fresh, handshaken one.
    pub(crate) fn connect(&self) -> Result<HerdrHello, HostError> {
        let mut slot = self.slot();
        *slot = None;
        let (connection, hello) = self.open()?;
        *slot = Some(connection);
        Ok(hello)
    }

    fn open(&self) -> Result<(Connection, HerdrHello), HostError> {
        let transport = |error: std::io::Error| HostError::Transport(error.to_string());
        let stream = UnixStream::connect(&self.socket_path).map_err(transport)?;
        stream
            .set_read_timeout(Some(self.config.io_timeout))
            .map_err(transport)?;
        stream
            .set_write_timeout(Some(self.config.io_timeout))
            .map_err(transport)?;
        let generation = self.generations.fetch_add(1, Ordering::SeqCst) + 1;
        let connected_at = SystemTime::now();
        let peer_pid = super::provenance::socket_peer_pid(&stream);
        let mut connection = Connection {
            reader: BufReader::new(stream.try_clone().map_err(transport)?),
            writer: stream,
            generation,
            hello: HerdrHello {
                version: String::new(),
                protocol: 0,
            },
            peer_pid,
            connected_at,
        };
        let ping = HerdrCall {
            id: format!("adk-hello-{generation}"),
            request: HerdrRequest::Ping {},
        };
        let outcome = connection.exchange(&self.framing, &ping, self.config.max_frame_bytes);
        let hello = observe::hello_result(&ping, outcome)?;
        connection.hello = hello.clone();
        Ok((connection, hello))
    }

    /// Generation of the current connection, 0 when none is open. Test-only:
    /// callers take the generation from `call`, never from a later read.
    #[cfg(test)]
    fn generation(&self) -> u64 {
        self.slot()
            .as_ref()
            .map_or(0, |connection| connection.generation)
    }

    /// Mutations never open a connection: a replaced server is not written to.
    /// The generation is read under the same lock as the exchange; 0 if none ran.
    /// With `expected`, only that connection may carry the call.
    fn attempt(&self, call: &HerdrCall, expected: Option<u64>) -> (HerdrOutcome, u64) {
        let mut slot = self.slot();
        if slot.is_none() && expected.is_none() && call.request.is_read_only() {
            match self.open() {
                Ok((connection, _)) => *slot = Some(connection),
                Err(error) => {
                    return (Err(HerdrTransportError::NotSent(format!("{error:?}"))), 0);
                }
            }
        }
        let Some(connection) = slot.as_mut() else {
            let error = HerdrTransportError::NotSent("no herdr connection".to_string());
            return (Err(error), 0);
        };
        let generation = connection.generation;
        if let Some(expected) = expected.filter(|expected| *expected != generation) {
            let error = format!("connection {generation} is not the verified {expected}");
            return (Err(HerdrTransportError::NotSent(error)), generation);
        }
        let outcome = connection.exchange(&self.framing, call, self.config.max_frame_bytes);
        if !matches!(&outcome, Ok(reply) if reply.id == call.id) {
            *slot = None;
        }
        (outcome, generation)
    }
}

impl<F: HerdrFraming> HerdrTransport for HerdrSocketTransport<F> {
    fn call(&self, call: &HerdrCall) -> (HerdrOutcome, u64) {
        if !call.request.is_read_only() {
            return self.attempt(call, None);
        }
        let deadline = Instant::now() + self.config.read_deadline;
        observe::retry_read(deadline, self.config.retry_backoff, || {
            self.attempt(call, None)
        })
    }

    fn call_on(&self, call: &HerdrCall, generation: u64) -> (HerdrOutcome, u64) {
        self.attempt(call, Some(generation))
    }

    fn server_peer(&self) -> Result<ServerPeer, RestoreUnverified> {
        let mut slot = self.slot();
        if slot.is_none() {
            let (connection, _) = self.open().map_err(|_| RestoreUnverified::NoPeer)?;
            *slot = Some(connection);
        }
        let connection = slot.as_ref().ok_or(RestoreUnverified::NoPeer)?;
        Ok(ServerPeer {
            generation: connection.generation,
            pid: connection.peer_pid.ok_or(RestoreUnverified::NoPeer)?,
            version: connection.hello.version.clone(),
            connected_at: connection.connected_at,
        })
    }
}

#[cfg(test)]
#[path = "transport_tests.rs"]
mod tests;
