//! T-H7 for a home this node never activated: with no O store and no session bound here the real
//! drain leaves, and one whose first init committed but whose actor never ran keeps waiting.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use super::drain_pg::{WITHIN, drains_without_leaving, epoch_of, reclaim_while, until_left};
use super::harness::*;
use crate::services::tui_o::store::STORE_DIR_NAME;
use crate::services::tui_o::writer::WriterAlarm;
use crate::services::tui_prompt_dedupe::binding_events as p5;

/// This node's P5 binding-log root, installed on the thread its drain runs on.
struct P5Root {
    root: PathBuf,
    saved: Option<PathBuf>,
}

impl P5Root {
    fn install() -> Self {
        let flavor = tokio::runtime::Handle::current().runtime_flavor();
        assert_eq!(flavor, tokio::runtime::RuntimeFlavor::CurrentThread);
        let root = crate::config::runtime_root().expect("node root");
        let saved = p5::test_root();
        p5::set_test_root(Some(&root));
        Self { root, saved }
    }

    /// The drain's reads saw this root: its watch shares this thread.
    fn still_installed(&self) {
        assert_eq!(p5::test_root().as_deref(), Some(self.root.as_path()));
    }
}

impl Drop for P5Root {
    fn drop(&mut self) {
        p5::set_test_root(self.saved.as_deref());
    }
}

fn no_store(root: &Path) -> bool {
    let dir = root.join(STORE_DIR_NAME).join(C.to_string());
    let entry = std::fs::symlink_metadata(dir).map_err(|error| error.kind());
    entry.err() == Some(std::io::ErrorKind::NotFound)
}

/// A delegated home whose writer never started here leaves on reclaim.
#[tokio::test(flavor = "current_thread")]
async fn th7_fresh_unhosted_reclaim_pg() {
    let (scene, _) = reclaim_while("th7_fresh_unhosted", Duration::from_secs(120)).await;
    scene.drop_db().await;
}

pub(super) async fn fresh_unhosted_node(env: &Env) {
    let p5 = P5Root::install();
    let epoch = epoch_of(env);
    let knobs = HostKnobs {
        unhosted: true,
        ..HostKnobs::default()
    };
    let node = Booted::boot_reading_with(env, worker_restored(), knobs).await;
    until("the lease opens the home", WITHIN, || {
        node.home_epoch() == Some(epoch)
    })
    .await;
    signal(&env.dir, "owed_seen");
    wait_signal(&env.dir, "reclaim_begun", WITHIN).await;
    until_left(&node).await;
    assert!(no_store(&node.root));
    assert_eq!(node.discord().posts(), Vec::<String>::new());
    p5.still_installed();
    node.stop().await;
}

/// A delegated home whose first activation waits on its facts leaves on reclaim; the activation
/// let through afterwards finds the gate retired and waits for ownership, creating nothing.
#[tokio::test(flavor = "current_thread")]
async fn th7_fresh_parked_reclaim_pg() {
    let (scene, _) = reclaim_while("th7_fresh_parked", Duration::from_secs(120)).await;
    scene.drop_db().await;
}

pub(super) async fn fresh_parked_node(env: &Env) {
    let p5 = P5Root::install();
    let epoch = epoch_of(env);
    let facts = Arc::new(Park::default());
    let knobs = HostKnobs {
        facts: Some(Arc::clone(&facts)),
        ..HostKnobs::default()
    };
    let node = Booted::boot_reading_with(env, worker_restored(), knobs).await;
    until("the lease opens the home", WITHIN, || {
        node.home_epoch() == Some(epoch)
    })
    .await;
    until("the first activation waits on its facts", WITHIN, || {
        facts.entered() == 1
    })
    .await;
    signal(&env.dir, "owed_seen");
    wait_signal(&env.dir, "reclaim_begun", WITHIN).await;
    until_left(&node).await;
    assert!(no_store(&node.root), "the drain left before any init");
    facts.release();
    until("the facts read returns", WITHIN, || facts.passed() == 1).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(
        facts.entered(),
        1,
        "no second read: the host waits on ownership"
    );
    assert!(
        !node
            .writers
            .iter()
            .all(tokio::task::JoinHandle::is_finished)
    );
    assert!(no_store(&node.root), "nothing activated after the leave");
    let halted = node.raised().into_iter();
    let halted: Vec<_> = halted
        .filter(|alarm| matches!(alarm, WriterAlarm::Halted { .. }))
        .collect();
    assert_eq!(halted, []);
    assert_eq!(node.discord().posts(), Vec::<String>::new());
    p5.still_installed();
    node.stop().await;
}

/// A home whose first init committed but whose actor never started owes unknown: it does not
/// leave until the actor runs and reads nothing owed.
#[tokio::test(flavor = "current_thread")]
async fn th7_store_without_actor_holds_pg() {
    let (scene, _) = reclaim_while("th7_store_without_actor", Duration::from_secs(180)).await;
    scene.drop_db().await;
}

pub(super) async fn store_without_actor_node(env: &Env) {
    let p5 = P5Root::install();
    let epoch = epoch_of(env);
    let port = Arc::new(Park::default());
    let knobs = HostKnobs {
        port: Some(Arc::clone(&port)),
        ..HostKnobs::default()
    };
    let node = Booted::boot_reading_with(env, worker_restored(), knobs).await;
    until("the lease opens the home", WITHIN, || {
        node.home_epoch() == Some(epoch)
    })
    .await;
    until(
        "the init commits and the host waits on its port",
        WITHIN,
        || port.entered() == 1 && !no_store(&node.root),
    )
    .await;
    signal(&env.dir, "owed_seen");
    drains_without_leaving(env, &node, epoch, &["owed_unreadable"]).await;
    port.release();
    until_left(&node).await;
    assert_eq!(port.passed(), 1);
    assert_eq!(node.discord().posts(), Vec::<String>::new());
    p5.still_installed();
    node.stop().await;
}
