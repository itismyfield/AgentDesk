use super::*;
use std::future::Future;
use std::io::Write;
use std::net::SocketAddr;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::time::{Duration, timeout};

const CHILD_ADDRESS: &str = "ADK_WRITER_AUTHORITY_TEST_PARENT";
const HEALTH_REQUEST: &[u8] = b"GET /api/health HTTP/1.1\r\nHost: localhost\r\n\r\n";

#[test]
fn codex_writer_yields_during_source_authority_io() {
    if let Ok(parent) = std::env::var(CHILD_ADDRESS) {
        writer_child(parent.parse().unwrap());
        return;
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(drive_child());
}

async fn drive_child() {
    let control = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let log = tempfile::NamedTempFile::new_in(scratch.path()).unwrap();
    let (_, module) = module_path!().split_once("::").unwrap();
    let test_name = format!("{module}::codex_writer_yields_during_source_authority_io");
    let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &test_name, "--nocapture"])
        .env(CHILD_ADDRESS, control.local_addr().unwrap().to_string())
        .env("TMPDIR", scratch.path())
        .stdout(Stdio::from(log.as_file().try_clone().unwrap()))
        .stderr(Stdio::from(log.as_file().try_clone().unwrap()))
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let result = probe_child(control, &mut child).await;
    if let Err(error) = result {
        if child.try_wait().unwrap().is_none() {
            child.kill().await.unwrap();
        }
        panic!("{error}\n{}", std::fs::read_to_string(log.path()).unwrap());
    }
}

async fn probe_child(
    control: TcpListener,
    child: &mut tokio::process::Child,
) -> Result<(), String> {
    let addresses = timeout(Duration::from_secs(10), async {
        let (control, _) = control.accept().await.unwrap();
        let mut addresses = String::new();
        BufReader::new(control)
            .read_line(&mut addresses)
            .await
            .unwrap();
        addresses
    })
    .await
    .map_err(|_| "writer never reached its rescope attempt")?;
    let (health, release) = addresses
        .trim()
        .split_once(' ')
        .ok_or("missing child listeners")?;
    let reply = timeout(Duration::from_secs(3), async {
        let mut health = TcpStream::connect(health).await.unwrap();
        health.write_all(HEALTH_REQUEST).await.unwrap();
        let mut reply = String::new();
        health.read_to_string(&mut reply).await.unwrap();
        reply
    })
    .await
    .map_err(|_| "writer stopped the runtime health response and timer during source-authority contention")?;
    if !reply.starts_with("HTTP/1.1 200 OK") {
        return Err(format!("health and timer did not advance: {reply:?}"));
    }
    TcpStream::connect(release)
        .await
        .unwrap()
        .write_all(b"x")
        .await
        .unwrap();
    let status = timeout(Duration::from_secs(10), child.wait())
        .await
        .map_err(|_| "writer did not finish after authority release")?
        .map_err(|error| error.to_string())?;
    if !status.success() {
        return Err(format!("writer child failed: {status}"));
    }
    Ok(())
}

fn writer_child(parent: SocketAddr) {
    use crate::services::{hang_forensics, tmux_common};

    let env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let dedupe_lock = DedupeGuard::acquire();
    let (root, env) = dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
    let scope = ScopeFixture::new();
    let fake_tmux =
        crate::services::provider_teardown::tests::test_support::FakeTmux::install(CANARY_TMUX);
    dedupe::reset_state_for_tests();
    dedupe::binding_events::set_test_root(Some(root.path()));
    let (original, source) = proven(root.path());
    let harness = Harness::build_channel(CANARY_CHANNEL, |_| {
        let bytes = std::fs::read(&source.path).unwrap();
        vec![InitSource {
            source_id: source.clone(),
            delivery_start: bytes.len() as u64,
            prefix_hash: hex::encode(Sha256::digest(&bytes)),
        }]
    });
    harness.gate.acquired();
    let checkpoint = dedupe::binding_events::binding_events_since(CANARY_CHANNEL, 0)
        .unwrap()
        .last()
        .unwrap()
        .seq;
    harness
        .channel()
        .set_binding_checkpoint(checkpoint)
        .unwrap();
    let writer = harness.writer();
    let selection = writer.suspension.clone();
    let (stop, stopped) = watch::channel(false);
    let (resumed, mut ready) = watch::channel(false);
    let mut boot_ready = ready.clone();
    let mut control = std::net::TcpStream::connect(parent).unwrap();
    let event_root = root.path().to_path_buf();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .on_thread_start(move || dedupe::binding_events::set_test_root(Some(&event_root)))
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let health = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let release = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addresses = format!(
            "{} {}\n",
            health.local_addr().unwrap(),
            release.local_addr().unwrap()
        );
        assert_eq!(
            hang_forensics::spawn_runtime_liveness_beacon_with_period(Duration::from_millis(10))
                .workers(),
            Some(1)
        );
        let selection_during_hold = selection.clone();
        let original_nonce = original.execution_nonce.clone();
        let health = tokio::spawn(async move {
            let (mut client, _) = health.accept().await.unwrap();
            let mut request = [0; HEALTH_REQUEST.len()];
            client.read_exact(&mut request).await.unwrap();
            assert_eq!(request.as_slice(), HEALTH_REQUEST);
            let ticks = hang_forensics::snapshot().runtime_ticks;
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert!(hang_forensics::snapshot().runtime_ticks > ticks);
            assert_eq!(
                selection_during_hold.nonce().as_deref(),
                Some(original_nonce.as_str())
            );
            client
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
        });

        let (booted, boot) = oneshot::channel();
        let (poll_writer, poll) = oneshot::channel();
        let actor = tokio::spawn(async move {
            let mut writer = Box::pin(run_channel(
                writer,
                ShadowProvider::Codex,
                Arc::new(ChannelBindingLog::new(
                    CANARY_CHANNEL,
                    ShadowProvider::Codex,
                )),
                stopped,
                resumed,
            ));
            std::future::poll_fn(|cx| {
                assert!(writer.as_mut().poll(cx).is_pending());
                if *boot_ready.borrow_and_update() {
                    std::task::Poll::Ready(())
                } else {
                    std::task::Poll::Pending
                }
            })
            .await;
            booted.send(()).unwrap();
            poll.await.unwrap();
            crate::services::tui_o::writer::actor::BEFORE_SCOPE_ATTEMPT.with_borrow_mut(|hook| {
                *hook = Some(Box::new(move || {
                    control.write_all(addresses.as_bytes()).unwrap()
                }));
            });
            writer.await
        });
        boot.await.unwrap();
        assert_eq!(
            selection.nonce().as_deref(),
            Some(original.execution_nonce.as_str())
        );
        let (successor, _) = proven(root.path());
        let (locked, lock) = oneshot::channel();
        let holder = tokio::task::spawn_blocking(move || {
            tmux_common::with_tmux_source_authority(CANARY_TMUX, |_| {
                tokio::runtime::Handle::current().block_on(async move {
                    let mut accept = std::pin::pin!(release.accept());
                    std::future::poll_fn(|cx| {
                        assert!(accept.as_mut().poll(cx).is_pending());
                        std::task::Poll::Ready(())
                    })
                    .await;
                    locked.send(()).unwrap();
                    let (mut socket, _) = accept.await.unwrap();
                    socket.read_exact(&mut [0]).await.unwrap();
                });
            });
        });
        lock.await.unwrap();
        poll_writer.send(()).unwrap();
        health.await.unwrap();
        holder.await.unwrap();
        ready
            .wait_for(|ready| {
                *ready && selection.nonce().as_deref() == Some(successor.execution_nonce.as_str())
            })
            .await
            .unwrap();
        assert!(selection.contains(&source));
        stop.send_replace(true);
        assert_eq!(actor.await.unwrap(), None);
    });
    drop((fake_tmux, scope, env, dedupe_lock, env_lock));
}
