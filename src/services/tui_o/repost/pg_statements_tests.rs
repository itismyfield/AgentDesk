//! A PostgreSQL wire proxy for one pool: while armed it records every statement the pool runs on
//! the re-post tables, by text, whether sent simple or prepared.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[derive(Default)]
pub(super) struct Statements {
    pub(super) armed: AtomicBool,
    pub(super) simple: AtomicUsize,
    pub(super) execute: AtomicUsize,
    texts: Mutex<Vec<String>>,
}

impl Statements {
    pub(super) fn count(&self) -> usize {
        self.matching(&[])
    }

    /// Clears what was recorded and starts recording.
    pub(super) fn reset(&self) {
        self.simple.store(0, Ordering::SeqCst);
        self.execute.store(0, Ordering::SeqCst);
        self.texts.lock().unwrap().clear();
        self.armed.store(true, Ordering::SeqCst);
    }

    /// The recorded statements whose text contains every one of `parts`.
    pub(super) fn matching(&self, parts: &[&str]) -> usize {
        let texts = self.texts.lock().unwrap();
        let matches = |text: &&String| parts.iter().all(|part| text.contains(part));
        texts.iter().filter(matches).count()
    }
}

/// The statement's text when it touches a re-post table.
fn repost_sql(sql: &[u8]) -> Option<String> {
    let sql = String::from_utf8_lossy(sql);
    ["o_piece_delivery", "o_piece_attempts", "o_piece_receipts"]
        .iter()
        .any(|table| sql.contains(table))
        .then(|| sql.into_owned())
}

async fn observe_pg(mut client: TcpStream, mut server: TcpStream, counts: Arc<Statements>) {
    let Ok(length) = client.read_u32().await else {
        return;
    };
    let mut startup = vec![0; length as usize - 4];
    if client.read_exact(&mut startup).await.is_err()
        || server.write_u32(length).await.is_err()
        || server.write_all(&startup).await.is_err()
    {
        return;
    }
    let (mut from_client, mut to_client) = client.into_split();
    let (mut from_server, mut to_server) = server.into_split();
    let uplink = async move {
        let mut statements = BTreeMap::<Vec<u8>, Option<String>>::new();
        let mut portals = BTreeMap::<Vec<u8>, Option<String>>::new();
        while let Ok(kind) = from_client.read_u8().await {
            let length = from_client.read_u32().await?;
            let mut body = vec![0; length as usize - 4];
            from_client.read_exact(&mut body).await?;
            let mut fields = body.split(|byte| *byte == 0);
            let first = fields.next().unwrap_or_default();
            let second = fields.next().unwrap_or_default();
            let dedicated = match kind {
                b'P' => {
                    statements.insert(first.to_vec(), repost_sql(second));
                    None
                }
                b'B' => {
                    let text = statements.get(second).cloned().flatten();
                    portals.insert(first.to_vec(), text);
                    None
                }
                b'E' => portals.get(first).cloned().flatten(),
                b'Q' => repost_sql(first),
                _ => None,
            };
            if let Some(text) = dedicated.filter(|_| counts.armed.load(Ordering::SeqCst)) {
                let count = if kind == b'Q' {
                    &counts.simple
                } else {
                    &counts.execute
                };
                count.fetch_add(1, Ordering::SeqCst);
                counts.texts.lock().unwrap().push(text);
            }
            to_server.write_u8(kind).await?;
            to_server.write_u32(length).await?;
            to_server.write_all(&body).await?;
        }
        Ok::<_, std::io::Error>(())
    };
    let downlink = async move {
        tokio::io::copy(&mut from_server, &mut to_client)
            .await
            .map(|_| ())
    };
    let _ = tokio::try_join!(uplink, downlink);
}

/// A one-connection pool to `url` through the proxy, its record, and the proxy task.
pub(super) async fn observed_pool(
    url: &str,
) -> (sqlx::PgPool, Arc<Statements>, tokio::task::JoinHandle<()>) {
    let options: sqlx::postgres::PgConnectOptions = url.parse().unwrap();
    let upstream = (options.get_host().to_owned(), options.get_port());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let counts = Arc::new(Statements::default());
    let observed = Arc::clone(&counts);
    let proxy = tokio::spawn(async move {
        while let Ok((client, _)) = listener.accept().await {
            let server = TcpStream::connect((upstream.0.as_str(), upstream.1))
                .await
                .unwrap();
            tokio::spawn(observe_pg(client, server, Arc::clone(&observed)));
        }
    });
    let options = options
        .host("127.0.0.1")
        .port(port)
        .ssl_mode(sqlx::postgres::PgSslMode::Disable);
    let pool = sqlx::postgres::PgPoolOptions::new()
        .min_connections(1)
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    (pool, counts, proxy)
}
