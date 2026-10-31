//! A loopback PostgreSQL wire proxy for health tests: it delays every client message by a fixed
//! one-way latency, records each executed statement, and can hold statements matching a needle.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

#[derive(Default)]
struct Shared {
    delay: Mutex<Duration>,
    stalls: Mutex<Vec<String>>,
    executions: Mutex<Vec<String>>,
    accepted: AtomicUsize,
    open: AtomicUsize,
}

/// One proxy in front of one upstream database; dropping it stops accepting connections.
pub(crate) struct PgWireProxy {
    port: u16,
    shared: Arc<Shared>,
    release: watch::Sender<u64>,
    accept: tokio::task::JoinHandle<()>,
    upstream_options: sqlx::postgres::PgConnectOptions,
}

impl Drop for PgWireProxy {
    fn drop(&mut self) {
        self.accept.abort();
        self.release.send_modify(|generation| *generation += 1);
    }
}

impl PgWireProxy {
    /// Proxies `database_url`, adding `delay` to every client message (so to every round trip).
    pub(crate) async fn start(database_url: &str, delay: Duration) -> Self {
        let upstream_options: sqlx::postgres::PgConnectOptions =
            database_url.parse().expect("test database url parses");
        let upstream = (
            upstream_options.get_host().to_owned(),
            upstream_options.get_port(),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind proxy");
        let port = listener.local_addr().expect("proxy addr").port();
        let shared = Arc::new(Shared::default());
        *shared.delay.lock().unwrap() = delay;
        let (release, _) = watch::channel(0u64);
        let accept = {
            let shared = Arc::clone(&shared);
            let release = release.clone();
            tokio::spawn(async move {
                while let Ok((client, _)) = listener.accept().await {
                    let Ok(server) = TcpStream::connect((upstream.0.as_str(), upstream.1)).await
                    else {
                        continue;
                    };
                    let _ = client.set_nodelay(true);
                    let _ = server.set_nodelay(true);
                    tokio::spawn(relay(
                        client,
                        server,
                        Arc::clone(&shared),
                        release.subscribe(),
                    ));
                }
            })
        };
        Self {
            port,
            shared,
            release,
            accept,
            upstream_options,
        }
    }

    /// A pool through the proxy with the runtime pool's `test_before_acquire` behavior.
    pub(crate) fn pool(&self, max_connections: u32) -> sqlx::PgPool {
        let options = self
            .upstream_options
            .clone()
            .host("127.0.0.1")
            .port(self.port)
            .ssl_mode(sqlx::postgres::PgSslMode::Disable);
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(max_connections)
            .min_connections(0)
            .acquire_timeout(Duration::from_secs(10))
            .test_before_acquire(true)
            .connect_lazy_with(options)
    }

    pub(crate) fn set_delay(&self, delay: Duration) {
        *self.shared.delay.lock().unwrap() = delay;
    }

    /// Holds every later execution whose SQL contains `needle` until [`Self::release`].
    pub(crate) fn stall(&self, needle: &str) {
        self.shared.stalls.lock().unwrap().push(needle.to_string());
    }

    /// Clears every stall and lets held executions through.
    pub(crate) fn release(&self) {
        self.shared.stalls.lock().unwrap().clear();
        self.release.send_modify(|generation| *generation += 1);
    }

    /// SQL texts of the Execute and simple Query messages seen since the last reset.
    /// A `test_before_acquire` ping is a bare Sync, so it is not counted.
    pub(crate) fn executions(&self) -> Vec<String> {
        self.shared.executions.lock().unwrap().clone()
    }

    pub(crate) fn reset_executions(&self) {
        self.shared.executions.lock().unwrap().clear();
    }

    /// PostgreSQL v3 startups accepted since the proxy started.
    pub(crate) fn accepted(&self) -> usize {
        self.shared.accepted.load(Ordering::SeqCst)
    }

    /// PostgreSQL connections still being relayed.
    pub(crate) fn open(&self) -> usize {
        self.shared.open.load(Ordering::SeqCst)
    }
}

fn stalled(shared: &Shared, sql: &str) -> bool {
    shared
        .stalls
        .lock()
        .unwrap()
        .iter()
        .any(|needle| sql.contains(needle.as_str()))
}

type Queued = (tokio::time::Instant, Vec<u8>, Option<String>);

async fn read_startup(
    from_client: &mut (impl tokio::io::AsyncRead + Unpin),
) -> std::io::Result<Vec<u8>> {
    let length = from_client.read_u32().await?;
    // PostgreSQL caps the startup payload at 10,000 bytes, excluding its length word.
    if !(8..=10_000 + 4).contains(&length) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid PostgreSQL startup length",
        ));
    }
    let protocol = from_client.read_u32().await?;
    // Pools returned by this fixture disable TLS and send a plain protocol 3.0 startup.
    if protocol != 3 << 16 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "unsupported PostgreSQL startup protocol",
        ));
    }
    let mut startup = length.to_be_bytes().to_vec();
    startup.extend_from_slice(&protocol.to_be_bytes());
    startup.resize(length as usize, 0);
    from_client.read_exact(&mut startup[8..]).await?;
    Ok(startup)
}

/// Reads client frames, stamping each with its due time and the SQL an Execute or Query runs.
async fn read_client(
    mut from_client: tokio::net::tcp::OwnedReadHalf,
    queue: tokio::sync::mpsc::UnboundedSender<Queued>,
    shared: Arc<Shared>,
) -> std::io::Result<()> {
    let delay = || *shared.delay.lock().unwrap();
    let startup = read_startup(&mut from_client).await?;
    shared.accepted.fetch_add(1, Ordering::SeqCst);
    let _open = OpenConnection::count(&shared);
    let _ = queue.send((tokio::time::Instant::now() + delay(), startup, None));
    let mut statements = HashMap::<Vec<u8>, String>::new();
    let mut portals = HashMap::<Vec<u8>, String>::new();
    loop {
        let kind = from_client.read_u8().await?;
        let length = from_client.read_u32().await?;
        let mut frame = vec![kind];
        frame.extend_from_slice(&length.to_be_bytes());
        frame.resize(length as usize + 1, 0);
        from_client.read_exact(&mut frame[5..]).await?;
        let mut fields = frame[5..].split(|byte| *byte == 0);
        let first = fields.next().unwrap_or_default().to_vec();
        let second = fields.next().unwrap_or_default();
        let executed = match kind {
            b'P' => {
                statements.insert(first, String::from_utf8_lossy(second).into_owned());
                None
            }
            b'B' => {
                let text = statements.get(second).cloned().unwrap_or_default();
                portals.insert(first, text);
                None
            }
            b'E' => Some(portals.get(&first).cloned().unwrap_or_default()),
            b'Q' => Some(String::from_utf8_lossy(&first).into_owned()),
            _ => None,
        };
        let _ = queue.send((tokio::time::Instant::now() + delay(), frame, executed));
    }
}

/// Forwards each frame at its due time, holding a stalled execution until a release.
async fn write_server(
    mut to_server: tokio::net::tcp::OwnedWriteHalf,
    mut queued: tokio::sync::mpsc::UnboundedReceiver<Queued>,
    shared: Arc<Shared>,
    mut release: watch::Receiver<u64>,
) -> std::io::Result<()> {
    while let Some((due, frame, executed)) = queued.recv().await {
        tokio::time::sleep_until(due).await;
        if let Some(sql) = executed {
            while stalled(&shared, &sql) {
                if release.changed().await.is_err() {
                    return Ok(());
                }
            }
            shared.executions.lock().unwrap().push(sql);
        }
        to_server.write_all(&frame).await?;
    }
    Ok(())
}

async fn relay(
    client: TcpStream,
    server: TcpStream,
    shared: Arc<Shared>,
    release: watch::Receiver<u64>,
) {
    let (from_client, mut to_client) = client.into_split();
    let (mut from_server, to_server) = server.into_split();
    let (queue, queued) = tokio::sync::mpsc::unbounded_channel();
    let downlink = async move {
        tokio::io::copy(&mut from_server, &mut to_client)
            .await
            .map(|_| ())
    };
    let _ = tokio::try_join!(
        read_client(from_client, queue, Arc::clone(&shared)),
        write_server(to_server, queued, shared, release),
        downlink
    );
}

/// Counts one relayed connection as open until the relay ends.
struct OpenConnection(Arc<Shared>);

impl OpenConnection {
    fn count(shared: &Arc<Shared>) -> Self {
        shared.open.fetch_add(1, Ordering::SeqCst);
        Self(Arc::clone(shared))
    }
}

impl Drop for OpenConnection {
    fn drop(&mut self) {
        self.0.open.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn only_postgres_startups_are_forwarded_and_counted() {
        let malformed = [
            10_005u32.to_be_bytes().to_vec(),
            b"GET ".to_vec(),
            7u32.to_be_bytes().to_vec(),
            [8u32.to_be_bytes(), (2u32 << 16).to_be_bytes()].concat(),
        ];
        for bytes in malformed {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let mut client = TcpStream::connect(listener.local_addr().unwrap())
                .await
                .unwrap();
            let (peer, _) = listener.accept().await.unwrap();
            let (from_client, _) = peer.into_split();
            client.write_all(&bytes).await.unwrap();
            client.shutdown().await.unwrap();
            let (queue, mut queued) = tokio::sync::mpsc::unbounded_channel();
            let shared = Arc::new(Shared::default());
            let error = read_client(from_client, queue, shared.clone())
                .await
                .unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidData, "{bytes:?}");
            assert!(
                queued.try_recv().is_err(),
                "rejected bytes never reach PostgreSQL"
            );
            assert_eq!(shared.accepted.load(Ordering::SeqCst), 0);
            assert_eq!(shared.open.load(Ordering::SeqCst), 0);
        }

        for length in [8u32, 10_004] {
            let mut packet = [length.to_be_bytes(), (3u32 << 16).to_be_bytes()].concat();
            packet.resize(length as usize, 0);
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let mut client = TcpStream::connect(listener.local_addr().unwrap())
                .await
                .unwrap();
            let (peer, _) = listener.accept().await.unwrap();
            let (from_client, _) = peer.into_split();
            client.write_all(&packet).await.unwrap();
            client.shutdown().await.unwrap();
            let (queue, mut queued) = tokio::sync::mpsc::unbounded_channel();
            let shared = Arc::new(Shared::default());
            let error = read_client(from_client, queue, shared.clone())
                .await
                .unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
            assert_eq!(queued.recv().await.unwrap().1, packet);
            assert!(queued.try_recv().is_err());
            assert_eq!(shared.accepted.load(Ordering::SeqCst), 1);
            assert_eq!(shared.open.load(Ordering::SeqCst), 0);
        }
    }
}
