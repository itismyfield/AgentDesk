//! Boot preparation precedes custody and reaping, including through the public once wrapper.

use super::nondestructive_loader_tests::{CLAUDE, Env, STALE, row};
use super::*;

/// Episode directories under the boot custody root, by the channel each marker names.
fn custody_channels(env: &Env) -> Vec<u64> {
    let root = env.dir().with_file_name("discord_custody").join("claude");
    let dirs = fs::read_dir(root).into_iter().flatten().flatten();
    let mut channels: Vec<u64> = dirs
        .filter_map(|dir| {
            let marker = fs::read(dir.path().join("episode.json")).ok()?;
            let marker: serde_json::Value = serde_json::from_slice(&marker).ok()?;
            marker["episode"]["channel_id"].as_u64()
        })
        .collect();
    channels.sort_unstable();
    channels
}

/// Boot preparation settles before a caller may reserve the provider's pass: a caller held
/// in preparation neither copies nor unlinks, and a later-prepared caller runs the pass.
#[tokio::test]
async fn c2b_boot_preparation_settles_before_the_reaper_pass_is_reserved() {
    let env = Env::new();
    let legacy = 6_325_570;
    let legacy_path = env.seed(&row(legacy, None), STALE);
    let guard = BootReapOnce::default();
    let (release, latch) = tokio::sync::oneshot::channel::<()>();
    let held = reap_inflight_rows_after_preparation(&guard, &CLAUDE, None, async move {
        let _ = latch.await;
        BootPreparation::Unconfigured
    });
    tokio::pin!(held);
    for _ in 0..3 {
        assert!(futures::poll!(held.as_mut()).is_pending());
        tokio::task::yield_now().await;
    }
    assert!(legacy_path.exists(), "no unlink while preparation is held");
    assert!(custody_channels(&env).is_empty(), "no custody copy either");

    let prepared = reap_inflight_rows_at_boot_with_guard(&guard, &CLAUDE, None).await;
    assert!(
        !prepared.already_ran,
        "the held caller reserved no pass before preparing"
    );
    assert_eq!(prepared.reaped_stale, 1);
    assert_eq!(custody_channels(&env), vec![legacy]);

    release.send(()).unwrap();
    let late = held.await;
    assert!(late.already_ran, "the provider's pass ran once");
}

const PUBLIC_BOOT_CHILD: &str = "ADK_N3BC_D_PUBLIC_BOOT_CHILD";
const PUBLIC_BOOT_DONE: &str = "N3BC_D_PUBLIC_BOOT_DONE";

fn run_public_boot_in_fresh_child() {
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let qualified = format!(
        "{}::c2b_public_boot_preparation_and_access_attempts",
        module_path!().split_once("::").unwrap().1
    );
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &qualified, "--nocapture"])
        .env(PUBLIC_BOOT_CHILD, "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let child_pid = child.id();
    assert_ne!(child_pid, std::process::id());
    let read_pipe = |mut pipe: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            pipe.read_to_end(&mut bytes).unwrap();
            String::from_utf8_lossy(&bytes).into_owned()
        })
    };
    let stdout = read_pipe(Box::new(child.stdout.take().unwrap()));
    let stderr = read_pipe(Box::new(child.stderr.take().unwrap()));
    let deadline = Instant::now() + Duration::from_secs(20);
    let (status, timed_out) = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break (status, false);
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            break (child.wait().unwrap(), true);
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let (stdout, stderr) = (stdout.join().unwrap(), stderr.join().unwrap());
    assert!(
        !timed_out,
        "child watchdog expired; fixture completion unproven: {stdout}\n{stderr}"
    );
    assert!(status.success(), "child failed: {stdout}\n{stderr}");
    assert_eq!(
        stdout
            .lines()
            .filter(|line| *line == "running 1 test")
            .count(),
        1,
        "child must select exactly one test: {stdout}"
    );
    let summaries: Vec<_> = stdout
        .lines()
        .filter(|line| line.starts_with("test result:"))
        .collect();
    assert_eq!(summaries.len(), 1, "missing child summary: {stdout}");
    let counts: Vec<_> = summaries[0].split(';').map(str::trim).collect();
    assert_eq!(
        counts.get(..3),
        Some(["test result: ok. 1 passed", "0 failed", "0 ignored"].as_slice()),
        "child must execute one test and ignore none: {stdout}"
    );
    let markers: Vec<_> = stdout
        .lines()
        .filter(|line| line.contains(PUBLIC_BOOT_DONE))
        .collect();
    assert_eq!(markers.len(), 1, "fixture completion marker: {stdout}");
    assert_eq!(stdout.matches(PUBLIC_BOOT_DONE).count(), 1);
    let marker_pid = markers[0]
        .split("pid=")
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse::<u32>()
        .unwrap();
    assert_eq!(marker_pid, child_pid, "completion belongs to fresh child");
    for line in stdout.lines() {
        if let Some((_, marker)) = line.split_once("N3BC_D_") {
            println!("N3BC_D_{marker}");
        }
    }
    println!("N3BC_D_CHILD_VERIFIED executed=1 failed=0 ignored=0 pid={child_pid}");
}

#[tokio::test]
async fn c2b_public_boot_preparation_and_access_attempts() {
    use crate::services::discord::inflight::store::boot_observation::{Counts, Probe};
    use std::time::Duration;

    if std::env::var(PUBLIC_BOOT_CHILD).ok().as_deref() != Some("1") {
        run_public_boot_in_fresh_child();
        return;
    }
    let env = Env::new();
    let legacy = 6_325_574;
    let legacy_path = env.seed(&row(legacy, None), STALE);
    let pending_root =
        crate::services::discord::runtime_store::tui_direct_pending_start_root().unwrap();
    fs::create_dir_all(&pending_root).unwrap();
    let pending_path =
        |channel| pending_root.join(format!("claude_{channel}_{}.json", channel + 1));
    let legacy_pending = pending_path(legacy);
    let invalid_pending = b"{\"provider\":\"claude\"}";
    fs::write(&legacy_pending, invalid_pending).unwrap();
    let (release, latch) = tokio::sync::oneshot::channel();
    let runtime_root = crate::services::discord::runtime_store::runtime_root().unwrap();
    let probe = Probe::new(runtime_root, latch);
    let totals = |counts: &Counts| {
        let mut attempts = [counts.reservations, 0, 0, 0, 0];
        for (path, source) in &counts.sources {
            attempts[1] += source[0];
            if path.starts_with(env.dir()) {
                attempts[2] += source[1];
            } else if path.starts_with(&pending_root) {
                attempts[3] += source[1];
            }
            attempts[4] += source[2];
        }
        attempts
    };

    let first = reap_inflight_rows_at_boot_blocking(&CLAUDE, None);
    tokio::pin!(first);
    let first_poll = futures::poll!(first.as_mut());
    let held = probe.counts();
    assert_eq!(
        held.preparations, 1,
        "public caller entered actual preparer"
    );
    assert_eq!(
        totals(&held),
        [0; 5],
        "held preparation attempted reservation/lock/row read/pending read/unlink: {held:?}"
    );
    println!(
        "N3BC_D_PREPARATION_HELD preparations={} attempts={:?}",
        held.preparations,
        totals(&held)
    );
    assert!(
        first_poll.is_pending(),
        "first public caller is held in preparation"
    );
    assert!(legacy_path.exists());
    assert!(custody_channels(&env).is_empty());

    let prepared = tokio::time::timeout(
        Duration::from_secs(5),
        reap_inflight_rows_at_boot_blocking(&CLAUDE, None),
    )
    .await
    .expect("second public caller completes");
    assert!(!prepared.already_ran, "held caller reserved no pass");
    assert_eq!(prepared.reaped_stale, 1);
    let ran = probe.counts();
    assert_eq!((ran.preparations, ran.reservations), (2, 1));
    let accesses =
        |counts: &Counts, path: &Path| counts.sources.get(path).copied().unwrap_or_default();
    let legacy_row = accesses(&ran, &legacy_path);
    let legacy_record = accesses(&ran, &legacy_pending);
    assert!(
        legacy_row.iter().all(|count| *count > 0),
        "worker row attempts: {ran:?}"
    );
    assert!(legacy_record[1] > 0, "worker pending read: {ran:?}");
    assert_eq!(custody_channels(&env), vec![legacy]);
    assert!(!legacy_path.exists());
    assert!(legacy_pending.exists());
    assert_eq!(fs::read(&legacy_pending).unwrap(), invalid_pending);

    release.send(()).unwrap();
    let late = tokio::time::timeout(Duration::from_secs(5), first)
        .await
        .expect("first public caller completes after release");
    assert!(
        late.already_ran,
        "public provider once gate retained the completed pass"
    );
    assert_eq!(late.reaped_stale, 1);
    let settled = probe.counts();
    assert_eq!((settled.preparations, settled.reservations), (2, 1));
    assert_eq!(
        settled.sources, ran.sources,
        "late caller attempted no further accesses"
    );
    println!(
        "{PUBLIC_BOOT_DONE} pid={} preparations=2 reservations=1 \
         legacy_row={legacy_row:?} legacy_pending={legacy_record:?}",
        std::process::id()
    );
}
