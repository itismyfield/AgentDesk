//! A forced cancel or force-kill sent to a node that does not own the session reaches the owner
//! once over real TCP: the owner's own selection read binds the execution, the origin applies nothing.

use std::collections::BTreeMap;
use std::sync::atomic::AtomicUsize;

use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower::ServiceExt;

use super::*;
use crate::config::ClusterNodeConfig;
use crate::services::session_forwarding::home_stop::tests::loopback_seam;

const ORIGIN: &str = "origin-node";
const THIRD: &str = "UPDATE sessions SET instance_id = 'third-node' WHERE session_key = $1";

/// The rig for test `name` in its own child, on a current-thread runtime so every probe runs.
fn rig_forward(name: &str) -> Option<(herdr::P11TerminationRig, tokio::runtime::Runtime)> {
    if !herdr::in_child(&format!("{}::{name}", module_path!()), CHANNEL) {
        return None;
    }
    let rig = herdr::P11TerminationRig::boot_with(CHANNEL, false);
    rig.thread_installer()();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .on_thread_start(rig.thread_installer())
        .build()
        .unwrap();
    Some((rig, runtime))
}

/// One request as the owner's socket read it off the wire, and what the owner's router answered.
#[derive(Debug, Clone)]
struct Received {
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
    body: Value,
    answered: (StatusCode, Value),
}

/// The owner's socket: an edit run before its router reads the request, and whether the answer
/// is written back or the connection dropped once the owner applied it.
#[derive(Clone, Copy)]
struct Wire {
    before: Option<&'static str>,
    answer: bool,
}

const PLAIN: Wire = Wire {
    before: None,
    answer: true,
};

/// The owner node serving its production router on a real loopback socket, and an origin node
/// whose registry and trusted configuration name that socket, admitted by the loopback seam.
struct Forwarding {
    origin: axum::Router,
    received: Arc<Mutex<Vec<Received>>>,
    in_flight: Arc<AtomicUsize>,
    server: tokio::task::JoinHandle<()>,
    _seam: loopback_seam::LoopbackGuard,
}

impl Forwarding {
    async fn start(node: &Node, wire: Wire) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let seam = loopback_seam::allow_loopback_for_test(address);
        let owner = axum::Router::new().nest("/api", node.app_on(herdr::NODE));
        let (received, in_flight) = (Arc::default(), Arc::new(AtomicUsize::new(0)));
        let serving = Serving {
            router: owner,
            pool: node.pool.clone(),
            key: node.key.clone(),
            wire,
            received: Arc::clone(&received),
            in_flight: Arc::clone(&in_flight),
        };
        let server = tokio::spawn(serving.run(listener));
        let capabilities = json!({"agentdesk_api": {"base_url": format!("http://{address}"),
            "session_forwarding": true, "cancel_forwarding_v1": true}});
        let advertise = "INSERT INTO worker_nodes (instance_id, status, capabilities, last_heartbeat_at) VALUES ($1, 'online', $2, NOW())";
        sqlx::query(advertise)
            .bind(herdr::NODE)
            .bind(capabilities)
            .execute(&node.pool)
            .await
            .unwrap();
        node.sql(
            "UPDATE sessions SET instance_id = $2 WHERE session_key = $1",
            Some(herdr::NODE),
        )
        .await;
        let mut config = crate::config::Config::default();
        config.server.auth_token = Some(AUTH.into());
        let trusted = ClusterNodeConfig {
            max_concurrent_dispatches: None,
            trusted_forward_origin: Some(format!("http://{address}/")),
            allow_private_forwarding: true,
            allow_insecure_http_forwarding: true,
        };
        config.cluster.nodes.insert(herdr::NODE.into(), trusted);
        let (pool, registry) = (Some(node.pool.clone()), Some(node.registry.clone()));
        let origin = crate::server::routes::home_stop_test_app(config, pool, registry, ORIGIN);
        Self {
            origin,
            received,
            in_flight,
            server,
            _seam: seam,
        }
    }

    /// One request to the origin, as an operator sends it: no forwarding header.
    async fn post(&self, uri: &str, body: Value) -> (StatusCode, Value) {
        let bearer = format!("Bearer {AUTH}");
        let headers = [("authorization", bearer.as_str()), JSON];
        let post = crate::server::routes::home_stop_test_post;
        post(&self.origin, uri, &headers, &body.to_string()).await
    }

    fn received(&self) -> Vec<Received> {
        self.received.lock().unwrap().clone()
    }

    /// The single request the owner received: the operation's path and body, and the forwarding
    /// headers naming this origin, the session's owner and the shared bearer.
    fn only(&self, path: &str, body: Value, origin: (StatusCode, &Value)) -> Received {
        let received = self.received();
        let (status, reply) = origin;
        let reached = "one request reached the owner; the origin answered";
        assert_eq!(received.len(), 1, "{reached} {status} {reply}");
        let request = received[0].clone();
        assert_eq!(
            (request.method.as_str(), request.path.as_str()),
            ("POST", path)
        );
        assert_eq!(request.body, body);
        let header = |name: &str| request.headers.get(name).map(String::as_str);
        assert_eq!(header("x-agentdesk-forwarded-by"), Some(ORIGIN));
        assert_eq!(header("x-agentdesk-session-owner"), Some(herdr::NODE));
        let bearer = format!("Bearer {AUTH}");
        assert_eq!(header("authorization"), Some(bearer.as_str()));
        request
    }

    /// A probe asserting the owner is serving a request when it runs, then flagging `flag`.
    fn inside_owner(
        &self,
        flag: Arc<AtomicBool>,
        edit: Option<(&Node, &'static str)>,
    ) -> SettlementWindowProbe {
        let in_flight = Arc::clone(&self.in_flight);
        let edit = edit.map(|(node, sql)| (node.pool.clone(), node.key.clone(), sql));
        Box::new(move || {
            Box::pin(async move {
                assert_eq!(
                    in_flight.load(Ordering::SeqCst),
                    1,
                    "inside the owner's request"
                );
                if let Some((pool, key, sql)) = edit {
                    let done = sqlx::query(sql).bind(&key).execute(&pool).await.unwrap();
                    assert_eq!(done.rows_affected(), 1);
                }
                flag.store(true, Ordering::SeqCst);
            })
        })
    }
}

impl Drop for Forwarding {
    fn drop(&mut self) {
        self.server.abort();
    }
}

struct Serving {
    router: axum::Router,
    pool: sqlx::PgPool,
    key: String,
    wire: Wire,
    received: Arc<Mutex<Vec<Received>>>,
    in_flight: Arc<AtomicUsize>,
}

impl Serving {
    /// Reads each HTTP/1.1 request off its connection, hands it to the owner's router with the
    /// peer address, records it, and writes the answer back unless the wire drops it.
    async fn run(self, listener: tokio::net::TcpListener) {
        loop {
            let Ok((mut socket, peer)) = listener.accept().await else {
                return;
            };
            let Some((method, path, headers, body)) = read_request(&mut socket).await else {
                continue;
            };
            if let Some(edit) = self.wire.before {
                let done = sqlx::query(edit).bind(&self.key).execute(&self.pool).await;
                assert_eq!(done.unwrap().rows_affected(), 1);
            }
            let mut request = axum::http::Request::builder()
                .method(method.as_str())
                .uri(&path);
            for (name, value) in &headers {
                request = request.header(name, value);
            }
            let mut request = request.body(Body::from(body.clone())).unwrap();
            request.extensions_mut().insert(ConnectInfo(peer));
            self.in_flight.fetch_add(1, Ordering::SeqCst);
            let response = self.router.clone().oneshot(request).await.unwrap();
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            let status = response.status();
            let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            let answered = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            let body = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let request = Received {
                method,
                path,
                headers,
                body,
                answered: (status, answered),
            };
            self.received.lock().unwrap().push(request);
            if self.wire.answer {
                let reason = status.canonical_reason().unwrap_or("");
                let head = format!(
                    "HTTP/1.1 {} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    status.as_u16(),
                    bytes.len()
                );
                socket.write_all(head.as_bytes()).await.unwrap();
                socket.write_all(&bytes).await.unwrap();
            }
            drop(socket);
        }
    }
}

type Request = (String, String, BTreeMap<String, String>, Vec<u8>);

/// The method, path, lower-cased headers and `content-length` body of one request.
async fn read_request(socket: &mut tokio::net::TcpStream) -> Option<Request> {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 4096];
    let end = loop {
        if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            break end;
        }
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        bytes.extend_from_slice(&chunk[..read]);
    };
    let head = String::from_utf8(bytes[..end].to_vec()).ok()?;
    let mut lines = head.split("\r\n");
    let mut start = lines.next()?.split(' ');
    let (method, path) = (start.next()?.to_string(), start.next()?.to_string());
    let headers: BTreeMap<String, String> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    let length: usize = headers
        .get("content-length")
        .map_or(Ok(0), |l| l.parse())
        .ok()?;
    let mut body = bytes[end + 4..].to_vec();
    while body.len() < length {
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    Some((method, path, headers, body))
}

/// The owner root the row's record names.
async fn owner_root(node: &Node) -> String {
    let root =
        "SELECT hosted_execution->'owner'->>'runtime_root' FROM sessions WHERE session_key = $1";
    sqlx::query_scalar(root)
        .bind(&node.key)
        .fetch_one(&node.pool)
        .await
        .unwrap()
}

const E08_ROWS: [&str; 5] = [
    "forward-retires",
    "identity-after-owner-selection",
    "identity-before-owner-read",
    "receiver-fence-owner-moved",
    "response-lost",
];

// One force-kill sent to a non-owner reaches the owner once with its key, retry, reason and fence
// headers. The owner's selection read binds the execution; the origin only relays the answer.
#[test]
fn e08_non_owner_real_forward_once_pg() {
    let Some((rig, runtime)) = rig_forward("e08_non_owner_real_forward_once_pg") else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let body = json!({"retry": true, "reason": "e08 test"});
        let mut rows_ran = 0;
        let mut node = Node::bind_e08(&rig, herdr::HostedState::Bound).await;
        for name in E08_ROWS {
            node = Node::again_e08(node, &rig).await;
            let wire = match name {
                "identity-before-owner-read" => Wire {
                    before: Some(OWNER_MOVE),
                    answer: true,
                },
                "receiver-fence-owner-moved" => Wire {
                    before: Some(THIRD),
                    answer: true,
                },
                "response-lost" => Wire {
                    before: None,
                    answer: false,
                },
                _ => PLAIN,
            };
            let forwarding = Forwarding::start(&node, wire).await;
            node.start(51, "turn-a", Some(DISPATCH)).await;
            let untouched = node.killed(&rig).await;
            let (effects, selection) = (ran(), ran());
            match name {
                "identity-after-owner-selection" => {
                    let edit = Some((&node, OWNER_MOVE));
                    probe_before_selection(forwarding.inside_owner(selection.clone(), edit));
                }
                "receiver-fence-owner-moved" => {}
                _ => probe_before_postlude(forwarding.inside_owner(effects.clone(), None)),
            }
            let (status, reply) = forwarding.post(&path(&node.key), body.clone()).await;
            let received = forwarding.only(
                &format!("/api{}", path(&node.key)),
                body.clone(),
                (status, &reply),
            );
            let after = node.killed(&rig).await;
            let retired_once = |after: &Killed| {
                assert_eq!(after.closes(), 1, "{name}: the owner closed once");
                assert_eq!(after.session, kept("disconnected", None), "{name}");
                assert_eq!(
                    after.dispatches,
                    dispatches("failed", "dispatched"),
                    "{name}"
                );
                assert_eq!(
                    after.retries,
                    [("pending".to_string(), 1)],
                    "{name}: one retry"
                );
                assert_eq!(
                    (after.row.clone(), after.held),
                    (retired(), false),
                    "{name}"
                );
            };
            match name {
                "forward-retires" | "identity-before-owner-read" => {
                    assert_eq!(
                        (status, reply.clone()),
                        received.answered,
                        "{name}: relayed"
                    );
                    assert_eq!(
                        (status, reply["termination"].clone()),
                        (StatusCode::OK, result("retired", "retired")),
                        "{name}: {reply}"
                    );
                    assert!(reply["retry_dispatch_id"].is_string(), "{reply}");
                    assert!(
                        effects.load(Ordering::SeqCst),
                        "{name}: effects ran in the owner"
                    );
                    retired_once(&after);
                    let root = owner_root(&node).await;
                    let moved = name == "identity-before-owner-read";
                    assert_eq!(
                        root == MOVED_ROOT,
                        moved,
                        "{name}: the owner retired what it read: {root}"
                    );
                }
                "identity-after-owner-selection" => {
                    assert_eq!(
                        (status, reply.clone()),
                        received.answered,
                        "{name}: relayed"
                    );
                    let refused = (StatusCode::CONFLICT, result("refused", IDENTITY));
                    assert_eq!((status, reply["termination"].clone()), refused, "{reply}");
                    assert!(
                        selection.load(Ordering::SeqCst),
                        "the owner's selection was reached"
                    );
                    let row = Some((herdr::HostedState::Bound, herdr::EXECUTION.to_string()));
                    assert_eq!(after, Killed { row, ..untouched });
                    assert_eq!(recorded_root(&node, herdr::EXECUTION).await, MOVED_ROOT);
                    runs(&node, "turn-a").await;
                }
                "receiver-fence-owner-moved" => {
                    assert_eq!(
                        (status, reply.clone()),
                        received.answered,
                        "{name}: relayed"
                    );
                    assert_eq!(status, StatusCode::CONFLICT, "{reply}");
                    assert_eq!(reply["code"], "session_forward_owner_conflict", "{reply}");
                    assert_eq!(reply["context"]["database_owner_instance_id"], "third-node");
                    assert_eq!(after, untouched, "neither node moved anything");
                    runs(&node, "turn-a").await;
                }
                "response-lost" => {
                    assert_eq!(status, StatusCode::BAD_GATEWAY, "{reply}");
                    assert_eq!(reply["code"], "session_forward_failed", "{reply}");
                    let owner = (StatusCode::OK, result("retired", "retired"));
                    let answered = &received.answered;
                    assert_eq!((answered.0, answered.1["termination"].clone()), owner);
                    assert!(effects.load(Ordering::SeqCst), "the owner applied it once");
                    retired_once(&after);
                }
                _ => unreachable!("{name}"),
            }
            drop(forwarding);
            rows_ran += 1;
        }
        assert_eq!(rows_ran, E08_ROWS.len(), "every row ran");
        node.finish().await;
    });
}

const E07_ROWS: [&str; 2] = [
    "cancel-forward-retires",
    "cancel-identity-after-owner-selection",
];

// One forced cancel sent to a non-owner reaches the owner once. A refusal from the same owner
// is final: the origin rereads the owner, finds the same target, and sends nothing more.
#[test]
fn e07_non_owner_real_forward_once_pg() {
    let Some((rig, runtime)) = rig_forward("e07_non_owner_real_forward_once_pg") else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let (uri, body) = (format!("/turns/{CHANNEL}/cancel"), json!({"force": true}));
        let mut rows_ran = 0;
        let mut node = Node::bind(&rig, herdr::HostedState::Bound, Some(DISPATCH)).await;
        for name in E07_ROWS {
            node = rebind(node, &rig, Some(DISPATCH)).await;
            let forwarding = Forwarding::start(&node, PLAIN).await;
            node.start(51, "turn-a", Some(DISPATCH)).await;
            let untouched = node.effects(&rig).await;
            let flag = ran();
            if name == "cancel-identity-after-owner-selection" {
                let edit = Some((&node, OWNER_MOVE));
                probe_before_selection(forwarding.inside_owner(flag.clone(), edit));
            } else {
                probe_before_postlude(forwarding.inside_owner(flag.clone(), None));
            }
            let (status, reply) = forwarding.post(&uri, body.clone()).await;
            let received = forwarding.only(&format!("/api{uri}"), body.clone(), (status, &reply));
            assert!(flag.load(Ordering::SeqCst), "{name}: the owner's probe ran");
            let after = node.effects(&rig).await;
            if name == "cancel-forward-retires" {
                assert_eq!(
                    (status, reply.clone()),
                    received.answered,
                    "relayed: {reply}"
                );
                assert_eq!(status, StatusCode::OK, "{reply}");
                assert_eq!(
                    reply["termination"],
                    result("retired", "retired"),
                    "{reply}"
                );
                assert_eq!(after.closes(), 1, "the owner closed once");
                assert_eq!(after.session, disconnected());
                assert_eq!(after.dispatches, dispatches("cancelled", "dispatched"));
                assert_eq!((after.row, after.held), (retired(), false));
            } else {
                let owner = (StatusCode::CONFLICT, result("refused", IDENTITY));
                let answered = &received.answered;
                assert_eq!(termination(answered.0, &answered.1), owner, "{answered:?}");
                assert_eq!(status, StatusCode::CONFLICT, "{reply}");
                let error = "cancel owner rejected the request without changing";
                assert_eq!(reply["error"], error, "{reply}");
                let row = Some((herdr::HostedState::Bound, herdr::EXECUTION.to_string()));
                assert_eq!(after, Effects { row, ..untouched });
                runs(&node, "turn-a").await;
            }
            drop(forwarding);
            rows_ran += 1;
        }
        assert_eq!(rows_ran, E07_ROWS.len(), "every row ran");
        node.finish().await;
    });
}
