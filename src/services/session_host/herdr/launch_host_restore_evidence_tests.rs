use super::*;
use crate::db::dispatched_sessions::hosted_execution::HostedOwner;
use crate::services::herdr_launch::{clear_epoch, workspace_marker};
use crate::services::session_host::{HostCreate, HostKind, host_for};

fn nonce(provider: &str) -> String {
    format!("w1-{provider}")
}

pub(super) fn label(provider: &str) -> String {
    workspace_marker(&format!("adk-t19-{provider}"), &nonce(provider))
}

pub(super) fn create(
    server: &T19Server,
    host: &SocketHerdrLaunchHost,
    endpoint: HerdrLaunchEndpoint,
    witness: ServerWitness,
    provider: &str,
    script: &Path,
) -> String {
    let owner = HostedOwner {
        provider: provider.into(),
        discord_token_hash: "w1-fixture".into(),
        channel_id: "1479671301387059404".into(),
        logical_key: format!("adk-t19-{provider}"),
        owner_node: endpoint.execution_node.clone(),
        runtime_root: server.dir.display().to_string(),
    };
    let launching = clear_epoch::Launching::for_launch(&owner).unwrap();
    let permit = launching
        .scope()
        .check_epoch()
        .unwrap()
        .begin_effect()
        .unwrap();
    let request = HerdrCreateRequest {
        endpoint,
        label: workspace_marker(&owner.logical_key, &nonce(provider)),
        cwd: server.dir.join("work"),
        command: format!("/bin/sh {}", script.display()),
        restore_off_witness: witness,
    };
    let pane = match host_for(HostKind::Herdr)
        .create(HostCreate::herdr(host, &request, &permit))
        .into_herdr()
    {
        HerdrCreateOutcome::Created { pane_id } => pane_id,
        other => panic!("W1 actual S04 create {provider}: {other:?}"),
    };
    server
        .append(json!({"event":"w1_created", "owner":owner, "nonce":nonce(provider), "pane":pane}));
    pane
}

fn read(server: &T19Server, id: &str, request: HerdrRequest) -> Value {
    assert!(matches!(
        request,
        HerdrRequest::SessionSnapshot {} | HerdrRequest::PaneProcessInfo { .. }
    ));
    server.raw_call(
        &serde_json::to_value(HerdrCall {
            id: id.into(),
            request,
        })
        .unwrap(),
    )
}

pub(super) fn snapshot(server: &T19Server, phase: &str) -> Value {
    let result = read(
        server,
        &format!("w1-{phase}-snapshot"),
        HerdrRequest::SessionSnapshot {},
    );
    assert_eq!(result["type"], "session_snapshot");
    let snapshot = &result["snapshot"];
    assert_eq!(snapshot["protocol"], 22);
    assert!(!snapshot["panes"].as_array().unwrap().is_empty());
    let fields = snapshot.as_object().unwrap().keys().collect::<Vec<_>>();
    server.append(json!({"event":"w1_snapshot", "phase":phase, "generation":server.generation,
        "version":snapshot["version"], "fields":fields, "panes":snapshot["panes"], "workspaces":snapshot["workspaces"]}));
    snapshot.clone()
}

fn field<'a>(snapshot: &'a Value, list: &str, key: &str, id: &Value) -> &'a Value {
    let mut matches = snapshot[list]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| &row[key] == id);
    let row = matches
        .next()
        .unwrap_or_else(|| panic!("W1 {list} has no {key}={id}"));
    assert!(matches.next().is_none(), "W1 duplicate {key}={id}");
    row
}

fn pane<'a>(snapshot: &'a Value, pane_id: &str) -> &'a Value {
    field(snapshot, "panes", "pane_id", &json!(pane_id))
}

fn workspace<'a>(snapshot: &'a Value, pane: &Value) -> &'a Value {
    field(
        snapshot,
        "workspaces",
        "workspace_id",
        &pane["workspace_id"],
    )
}

fn send_fixture_input(server: &T19Server, id: &str, pane_id: &str, text: String) {
    assert_eq!(
        server.rpc(
            id,
            HerdrRequest::PaneSendInput {
                pane_id: pane_id.into(),
                text,
                keys: vec!["enter".into()],
            }
        ),
        HerdrResult::Ok
    );
    server.confirm_write(id, "pane.send_input");
}

pub(super) fn use_before_read(server: &T19Server, pane_id: &str) {
    let marker = server.dir.join("w1-used");
    assert!(!marker.exists());
    send_fixture_input(
        server,
        "w1-input-before-read",
        pane_id,
        format!("printf used > '{}'", marker.display()),
    );
    wait_for(
        || std::fs::read(&marker).is_ok_and(|bytes| bytes == b"used"),
        "first use before W1 observation",
    );
}

fn process_info(server: &T19Server, phase: &str, pane_id: &str) -> (u32, Value) {
    let id = format!("w1-{phase}-process");
    let result = read(
        server,
        &id,
        HerdrRequest::PaneProcessInfo {
            pane_id: pane_id.into(),
        },
    );
    assert_eq!(result["type"], "pane_process_info");
    let info = &result["process_info"];
    assert_eq!(info["pane_id"], pane_id);
    let shell = u32::try_from(info["shell_pid"].as_u64().unwrap()).unwrap();
    let foreground = info["foreground_processes"].as_array().unwrap();
    assert_eq!(foreground.len(), 1, "W1 shell foreground {phase}: {info}");
    assert_eq!(foreground[0]["pid"], shell);
    server.append(json!({"event":"w1_process", "phase":phase, "info":info}));
    (shell, info.clone())
}

pub(super) fn inspect(
    server: &mut T19Server,
    before: &Value,
    original_server: &ServerWitness,
    launched: &[(&str, String)],
    legacy_id: &str,
) {
    let after = snapshot(server, "restored");
    assert_ne!(&server.witness(), original_server);
    for (provider, id) in launched {
        let (old, restored) = (pane(before, id), pane(&after, id));
        assert_eq!(old["workspace_id"], restored["workspace_id"]);
        assert_eq!(workspace(&after, restored)["label"], label(provider));
        assert!(old["terminal_id"].is_string() && restored["terminal_id"].is_string());
        assert_ne!(old["terminal_id"], restored["terminal_id"]);
    }
    let first = pane(&after, &launched[0].1);
    let legacy = pane(&after, legacy_id);
    assert_eq!(
        workspace(&after, first)["label"],
        workspace(&after, legacy)["label"]
    );
    assert_ne!(first["workspace_id"], legacy["workspace_id"]);

    let used_id = &launched[1].1;
    let _ = process_info(server, "untouched", &launched[0].1);
    let (shell, idle) = process_info(server, "used-before-first-read", used_id);
    let marker = server.dir.join("w1-background-pid");
    send_fixture_input(
        server,
        "w1-input-background",
        used_id,
        format!(
            "/bin/sleep 120 & printf '%s\\n' $! > '{}'",
            marker.display()
        ),
    );
    let child = || {
        let bytes = std::fs::read_to_string(&marker).ok()?;
        bytes.strip_suffix('\n')?.parse::<u32>().ok()
    };
    wait_for(
        || child().is_some(),
        "background child PID written by scratch shell",
    );
    let child = child().unwrap();
    server.record(child, "w1-background");
    let parent = shell.to_string();
    assert_eq!(
        ps(child, "ppid="),
        parent,
        "background work really exists under this pane"
    );
    let (_, background) = process_info(server, "with-background-work", used_id);
    assert_eq!(
        background, idle,
        "the foreground reply does not see the child"
    );
    assert_eq!(
        ps(child, "ppid="),
        parent,
        "background work lived across the read"
    );
    send_fixture_input(
        server,
        "w1-end-background-fixture",
        used_id,
        format!("kill {child}; wait {child}"),
    );
    wait_for(|| ps(child, "pid=").is_empty(), "background fixture ended");
    let final_snapshot = snapshot(server, "after-reads");
    for id in [launched[0].1.as_str(), used_id.as_str(), legacy_id] {
        let _ = pane(&final_snapshot, id);
    }
}

pub(super) fn reused_id(server: &mut T19Server, session: &Path, before: &Value, original: &str) {
    std::fs::rename(session, session.with_extension("w1-original.json")).unwrap();
    server.start();
    let scratch = server.rpc(
        "w1-new-workspace",
        HerdrRequest::WorkspaceCreate {
            cwd: server.dir.join("work").display().to_string(),
            label: label("claude"),
            focus: false,
        },
    );
    let HerdrResult::WorkspaceCreated { root_pane } = scratch else {
        panic!("W1 new workspace: {scratch:?}")
    };
    assert_eq!(
        root_pane.pane_id, original,
        "fresh generation reuses this pane ID"
    );
    let info = server.process(original);
    server.record(info.shell_pid.unwrap(), "w1-fresh-shell");
    let fresh = snapshot(server, "id-reused");
    let (old, new) = (pane(before, original), pane(&fresh, original));
    assert_eq!(old["workspace_id"], new["workspace_id"]);
    assert_eq!(
        workspace(before, old)["label"],
        workspace(&fresh, new)["label"]
    );
    assert_ne!(old["terminal_id"], new["terminal_id"]);
    let log = std::fs::read_to_string(server.dir.join("home/xdg/herdr/herdr-server.log")).unwrap();
    for phase in ["before", "restored", "after-reads", "id-reused"] {
        assert!(
            log.contains(&format!("request_id=\"w1-{phase}-snapshot\"")),
            "W1 server request audit is missing {phase}"
        );
    }
    let closes = log
        .lines()
        .filter(|line| line.contains("method=\"pane.close\""))
        .count();
    assert_eq!(closes, 0);
    server.append(
        json!({"event":"w1_result", "lineage":"unknown", "inactivity":"unknown",
        "no_work":"unknown", "same_label_legacy":"ineligible", "reused_id":"ineligible",
        "used_before_read":"ineligible", "background_work":"ineligible", "pane_close":closes}),
    );
    assert!(
        server.stop(),
        "W1 fresh scratch server stops without escalation"
    );
}
