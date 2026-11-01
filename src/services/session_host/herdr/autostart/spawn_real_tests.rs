//! `ensure` starting a real protocol-22 herdr in an isolated HOME: once, detached, with an
//! allowlisted environment, and never beside a live server or a launcher about to start one.
use std::io::{BufRead, BufReader};
use std::os::unix::process::CommandExt;

use super::*;
use crate::services::session_host::autostart::spawn::{Context, Ensured, ensure_at};

fn context(home: &Home, grace_rounds: usize) -> Context {
    Context {
        env: home.env(),
        local_node: Some("node".into()),
        records: Some(home.records()),
        grace_rounds,
        lock_wait: Duration::from_secs(15),
        ready_wait: Duration::from_secs(10),
        supported: true,
    }
}

fn spawns(home: &Home) -> Vec<record::Spawn> {
    record::load(&home.records(), &home.socket())
        .unwrap()
        .spawns
}

fn ppid(pid: u32) -> Option<u32> {
    let out = Command::new("/bin/ps")
        .args(["-o", "ppid=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

fn alive(pid: u32) -> bool {
    // SAFETY: signal 0 only checks that the pid exists.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// The helper test's full name, as the nested libtest audit reads it.
const HELPER: &str =
    "services::session_host::herdr::autostart::real_tests::spawn::t7638_helper_ensure";

/// This test binary re-run as one more AgentDesk process calling `ensure` on `home`.
fn helper(home: &Home, hold: bool) -> Child {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", HELPER])
        .args(["--ignored", "--nocapture", "--test-threads=1"])
        .env("ADK_T7638_HELPER_HOME", &home.home)
        .env("ADK_T7638_HELPER_RECORDS", home.records())
        .env("ADK_T7638_HELPER_GO", home.records().with_extension("go"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if hold {
        command.env("ADK_T7638_HELPER_HOLD", "1");
    }
    // Its own process group, so the test can kill it the way AgentDesk's group would die.
    command.process_group(0);
    command.spawn().unwrap()
}

/// The rest of the first output line naming `prefix`; libtest may print it after the test name.
fn line(out: &mut BufReader<std::process::ChildStdout>, prefix: &str) -> String {
    let mut text = String::new();
    loop {
        text.clear();
        assert!(
            out.read_line(&mut text).unwrap() > 0,
            "helper ended before {prefix}"
        );
        if let Some(at) = text.find(prefix) {
            return text[at + prefix.len()..].trim().to_owned();
        }
    }
}

fn output(child: &mut Child) -> BufReader<std::process::ChildStdout> {
    BufReader::new(child.stdout.take().unwrap())
}

#[tokio::test]
#[ignore = "helper process of the autostart real tests; runs only with ADK_T7638_HELPER_HOME"]
async fn t7638_helper_ensure() {
    let Some(home) = std::env::var_os("ADK_T7638_HELPER_HOME").map(PathBuf::from) else {
        return;
    };
    let records = PathBuf::from(std::env::var_os("ADK_T7638_HELPER_RECORDS").unwrap());
    let go = PathBuf::from(std::env::var_os("ADK_T7638_HELPER_GO").unwrap());
    let context = Context {
        env: Environment {
            home: Some(home.clone()),
            path: Some(format!("{}:/usr/bin:/bin", home.join(".local/bin").display()).into()),
            excluded: vec![home.join(".adk")],
        },
        local_node: Some("node".into()),
        records: Some(records),
        grace_rounds: 0,
        lock_wait: Duration::from_secs(15),
        ready_wait: Duration::from_secs(10),
        supported: true,
    };
    println!("HELPER_READY");
    // Bounded, so a helper whose test was killed does not outlive it.
    let deadline = Instant::now() + Duration::from_secs(30);
    while !go.exists() && std::env::var_os("ADK_T7638_HELPER_HOLD").is_none() {
        assert!(Instant::now() < deadline, "no go signal");
        std::thread::sleep(Duration::from_millis(10));
    }
    let outcome = ensure_at(&home.join(".config/herdr/herdr.sock"), &context).await;
    println!("ENSURED {outcome:?}");
    if std::env::var_os("ADK_T7638_HELPER_HOLD").is_some() {
        std::thread::sleep(Duration::from_secs(120));
    }
}

#[tokio::test]
#[ignore = "needs ADK_TEST_HERDR_BIN: a protocol-22 herdr run in an isolated HOME"]
async fn t7638_an_absent_or_stale_default_server_is_started_detached_with_an_allowlisted_env() {
    let home = Home::new(FALSE);
    let socket = home.socket();
    let Ensured::Started(pid) = ensure_at(&socket, &context(&home, 0)).await else {
        panic!("{:?}", last(&socket));
    };
    wait_for(&socket);
    assert_eq!(home.herdr_pids(), vec![pid], "one server, the one recorded");
    let recorded = spawns(&home);
    assert_eq!(recorded.len(), 1);
    assert_eq!(
        (recorded[0].pid, recorded[0].outcome.as_str()),
        (Some(pid), "started")
    );
    // SAFETY: getsid only reads the session id of an existing pid.
    assert_eq!(
        unsafe { libc::getsid(pid as i32) },
        pid as i32,
        "its own session"
    );
    let env = crate::services::session_host::herdr::provenance::process_environ(pid).unwrap();
    let allowed = [
        "HOME",
        "PATH",
        "TERM",
        "USER",
        "LOGNAME",
        "SHELL",
        "TMPDIR",
        "LANG",
        "LC_ALL",
        "SSH_AUTH_SOCK",
    ];
    for entry in &env {
        let key = entry.split_once('=').map_or(entry.as_str(), |(key, _)| key);
        assert!(
            allowed.contains(&key),
            "{key} leaked from AgentDesk into the server: {env:?}"
        );
    }
    assert!(env.contains(&format!("HOME={}", home.home.display())));
    assert_eq!(last(&socket).unwrap()["reason"], "started");

    // SAFETY: kill signals this test's own server, which runs with its private HOME.
    unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(socket.exists(), "a killed server leaves its stale socket");
    let Ensured::Started(again) = ensure_at(&socket, &context(&home, 0)).await else {
        panic!("{:?}", last(&socket));
    };
    assert_ne!(again, pid);
    assert_eq!(spawns(&home).len(), 2);
}

#[tokio::test]
#[ignore = "needs ADK_TEST_HERDR_BIN: a protocol-22 herdr run in an isolated HOME"]
async fn t7638_a_live_server_is_never_started_again() {
    let mut home = Home::new(FALSE);
    let socket = home.socket();
    let pid = home.serve(&[], &["server"], &socket);
    let before = home.state();
    assert_eq!(
        ensure_at(&socket, &context(&home, 0)).await,
        Ensured::Present
    );
    assert_eq!(home.state(), before);
    assert_eq!(before.pids, vec![pid]);
    assert!(!home.records().exists(), "nothing recorded without a spawn");
}

#[tokio::test]
#[ignore = "needs ADK_TEST_HERDR_BIN: a protocol-22 herdr run in an isolated HOME"]
async fn t7638_what_is_not_an_absent_default_server_never_starts() {
    let resumes = Home::new("[session]\nresume_agents_on_restore = true\n");
    let before = resumes.state();
    assert_eq!(
        ensure_at(&resumes.socket(), &context(&resumes, 0)).await,
        Ensured::Refused("config_resume_not_off")
    );
    assert_eq!(resumes.state(), before);

    let file = Home::new(FALSE);
    std::fs::write(file.socket(), b"not a socket").unwrap();
    let before = file.state();
    assert_eq!(
        ensure_at(&file.socket(), &context(&file, 0)).await,
        Ensured::Refused("presence_unknown")
    );
    assert_eq!(file.state(), before);

    let named = Home::new(FALSE);
    let work = named.home.join(".config/herdr/sessions/work/herdr.sock");
    std::fs::create_dir_all(work.parent().unwrap()).unwrap();
    assert_eq!(
        ensure_at(&work, &context(&named, 0)).await,
        Ensured::Refused("socket_not_default")
    );
    for home in [&resumes, &file, &named] {
        assert!(home.herdr_pids().is_empty() && !home.records().exists());
    }
}

#[tokio::test]
#[ignore = "needs ADK_TEST_HERDR_BIN: a protocol-22 herdr run in an isolated HOME"]
async fn t7638_concurrent_demands_in_and_across_processes_start_one_server() {
    let mut home = Home::new(FALSE);
    std::fs::create_dir_all(home.records()).unwrap();
    let mut helpers = vec![helper(&home, false), helper(&home, false)];
    let mut outs: Vec<_> = helpers.iter_mut().map(output).collect();
    for out in &mut outs {
        line(out, "HELPER_READY");
    }
    std::fs::write(home.records().with_extension("go"), b"").unwrap();
    let socket = home.socket();
    let (one, two) = (context(&home, 0), context(&home, 0));
    let (first, second) = tokio::join!(ensure_at(&socket, &one), ensure_at(&socket, &two));
    let mut outcomes = vec![format!("{first:?}"), format!("{second:?}")];
    for out in &mut outs {
        outcomes.push(line(out, "ENSURED "));
    }
    home.others.extend(helpers);
    let started = outcomes
        .iter()
        .filter(|o| o.starts_with("Started("))
        .count();
    assert_eq!(started, 1, "{outcomes:?}");
    assert!(
        outcomes
            .iter()
            .all(|o| o.starts_with("Started(") || o == "Present"),
        "{outcomes:?}"
    );
    assert_eq!(spawns(&home).len(), 1, "{outcomes:?}");
    assert_eq!(home.herdr_pids().len(), 1);
}

#[tokio::test]
#[ignore = "needs ADK_TEST_HERDR_BIN: a protocol-22 herdr run in an isolated HOME"]
async fn t7638_the_server_outlives_the_process_group_that_started_it() {
    let mut home = Home::new(FALSE);
    let mut starter = helper(&home, true);
    let outcome = line(&mut output(&mut starter), "ENSURED ");
    let pid: u32 = outcome
        .strip_prefix("Started(")
        .and_then(|rest| rest.strip_suffix(')'))
        .and_then(|pid| pid.parse().ok())
        .unwrap_or_else(|| panic!("{outcome}"));
    // SAFETY: the helper leads its own process group, which only this test started.
    unsafe { libc::kill(-(starter.id() as i32), libc::SIGKILL) };
    let _ = starter.wait();
    std::thread::sleep(Duration::from_millis(300));
    assert!(alive(pid), "the server died with its starter");
    assert_eq!(
        ppid(pid),
        Some(1),
        "reparented, not a child of the dead starter"
    );
    wait_for(&home.socket());
    home.others.push(starter);
}

/// A process the scan sees as an idle TUI client of `home`: named `herdr`, no arguments, never
/// starting a server, as a client that lost its server and only reconnects.
fn idle_launcher(home: &Home) -> Child {
    let dir = home.records().with_extension("launcher");
    std::fs::create_dir_all(&dir).unwrap();
    let source = dir.join("idle.c");
    std::fs::write(
        &source,
        "#include <unistd.h>\nint main(void) { for (;;) pause(); }\n",
    )
    .unwrap();
    let built = Command::new("/usr/bin/cc")
        .arg(&source)
        .arg("-o")
        .arg(dir.join("herdr"))
        .status()
        .unwrap();
    assert!(built.success(), "cc builds the idle launcher stand-in");
    Command::new(dir.join("herdr"))
        .env_clear()
        .env("HOME", &home.home)
        .stdin(Stdio::null())
        .spawn()
        .unwrap()
}

#[tokio::test]
#[ignore = "needs ADK_TEST_HERDR_BIN: a protocol-22 herdr run in an isolated HOME"]
async fn t7638_an_idle_launcher_is_waited_out_then_one_server_starts() {
    let mut home = Home::new(FALSE);
    let launcher = idle_launcher(&home);
    let launcher_pid = launcher.id();
    home.others.push(launcher);
    std::thread::sleep(Duration::from_millis(200));
    let socket = home.socket();
    let Ensured::Started(pid) = ensure_at(&socket, &context(&home, 3)).await else {
        panic!("{:?}", last(&socket));
    };
    assert!(alive(launcher_pid), "the client is left alone");
    let mut expected = vec![launcher_pid, pid];
    expected.sort_unstable();
    assert_eq!(home.herdr_pids(), expected);
    assert_eq!(spawns(&home).len(), 1);
}

/// The real TUI client in a sized pseudo-terminal of its own session, which starts the server.
fn sized_client(home: &Home) -> Child {
    use std::os::fd::FromRawFd;
    let (mut leader, mut follower) = (0, 0);
    let mut size = libc::winsize {
        ws_row: 40,
        ws_col: 120,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: openpty writes two descriptors and reads `size`.
    let opened = unsafe {
        libc::openpty(
            &mut leader,
            &mut follower,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut size,
        )
    };
    assert_eq!(opened, 0);
    // SAFETY: openpty returned these descriptors open and unowned.
    let (leader, follower) = unsafe {
        (
            std::fs::File::from_raw_fd(leader),
            std::fs::File::from_raw_fd(follower),
        )
    };
    let mut command = Command::new(&home.bin);
    command
        .env_clear()
        .env("HOME", &home.home)
        .env("PATH", "/usr/bin:/bin")
        .env("TERM", "xterm-256color")
        .current_dir(&home.home)
        .stdin(follower.try_clone().unwrap())
        .stdout(follower.try_clone().unwrap())
        .stderr(follower);
    // SAFETY: setsid is async-signal-safe.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let child = command.spawn().unwrap();
    std::thread::spawn(move || {
        use std::io::Read;
        let mut leader = leader;
        let mut buffer = [0u8; 4096];
        while leader.read(&mut buffer).is_ok_and(|n| n > 0) {}
    });
    child
}

#[tokio::test]
#[ignore = "needs ADK_TEST_HERDR_BIN: a protocol-22 herdr run in an isolated HOME"]
async fn t7638_a_launcher_that_starts_its_server_in_the_grace_is_not_doubled() {
    let mut home = Home::new(FALSE);
    let client = sized_client(&home);
    home.others.push(client);
    let socket = home.socket();
    let outcome = ensure_at(&socket, &context(&home, presence::GRACE_ROUNDS)).await;
    assert!(
        matches!(
            outcome,
            Ensured::Present | Ensured::Refused("presence_unknown")
        ),
        "{outcome:?}"
    );
    assert!(
        !home.records().exists(),
        "AgentDesk spawned nothing: {outcome:?}"
    );
    wait_for(&socket);
}

#[tokio::test]
#[ignore = "needs ADK_TEST_HERDR_BIN: a protocol-22 herdr run in an isolated HOME"]
async fn t7638_a_fresh_launch_permit_starts_the_absent_server() {
    use crate::services::herdr_launch::HerdrLaunchHost;
    let home = Home::new(FALSE);
    let socket = home.socket();
    let _scoped =
        crate::services::session_host::autostart::spawn::scoped_context(&socket, context(&home, 0));
    let endpoint =
        super::super::super::model::HerdrEndpoint::new("node", "user", &socket, "default")
            .unwrap()
            .with_mode(crate::config::session_hosts::EndpointMode::UserDefault);
    let host = super::super::super::launch_host::SocketHerdrLaunchHost::new(
        vec![endpoint],
        super::super::super::transport::HerdrSocketConfig::default(),
    );
    let launch = crate::services::herdr_launch::HerdrLaunchEndpoint {
        execution_node: "node".into(),
        config_key: "user".into(),
        socket_addr: socket.display().to_string(),
        herdr_session: "default".into(),
    };
    let _ = host.create_permit(&launch).await;
    let recorded = spawns(&home);
    assert_eq!(recorded.len(), 1, "the permit asked for the server first");
    assert_eq!(home.herdr_pids(), vec![recorded[0].pid.unwrap()]);
}
