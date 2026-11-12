use super::*;
use crate::config::session_hosts::test_instance_validation::validate_hosts;
use serde_json::json;

pub(crate) fn fixture() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::Builder::new()
        .prefix("adk-i2-")
        .tempdir_in("/tmp")
        .unwrap();
    let root = temp
        .path()
        .canonicalize()
        .unwrap()
        .join(uuid::Uuid::new_v4().to_string());
    for directory in [
        "",
        "config",
        "runtime",
        "runtime/tmux",
        "runtime/herdr",
        "herdr-home",
        "home/.adk/profiles/claude/test-claude",
        "home/.adk/profiles/codex/test-codex",
        "tmp",
        "xdg",
        "fixture-worktree/target/debug",
    ] {
        std::fs::create_dir_all(root.join(directory)).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(root.join(directory), std::fs::Permissions::from_mode(0o700))
                .unwrap();
        }
    }
    let id = root.file_name().unwrap().to_str().unwrap();
    let manifest = json!({
        "schema_version":1,"status":"DECLARED","live_ready":false,"instance_id":id,
        "root":root,"worktree":root.join("fixture-worktree"),"head":"a".repeat(40),"profile":{
            "features":{},"bots":{},"channels":[]},"applied_env":{},
        "api":{"host":"127.0.0.1","port":18791,"self_url":"http://127.0.0.1:18791"},"postgres":{},
        "config_path":root.join("config/agentdesk.yaml"),"execution_node_id":id,
        "preferred_node_seed_required":true,"hosts":{
            "tmux":{"socket":root.join("runtime/tmux/socket"),"tmpdir":root.join("runtime/tmux"),
                "prefix":format!("AdkTest-{id}")},
            "herdr":{"socket":root.join("runtime/herdr/socket"),"home":root.join("herdr-home"),
                "session":format!("adk-test-{id}"),"endpoint_key":format!("test-{id}")}}
    });
    let mut receipt = manifest.as_object().unwrap().clone();
    receipt.remove("profile");
    receipt.remove("config_path");
    for key in ["features", "bots", "channels"] {
        receipt.insert(key.into(), manifest["profile"][key].clone());
    }
    write(&root.join("manifest.json"), &manifest);
    write(&root.join("receipt.json"), &Value::Object(receipt));
    write(
        &root.join("config/agentdesk.yaml"),
        &json!({"cluster":{
        "instance_id":id,"gateway_preferred_instance_id":id},
        "server":{"host":"127.0.0.1","port":18791},"mcp_servers":{},
        "memory":{"backend":"file","mcp":{"endpoint":""}}}),
    );
    let mut profiles = serde_json::Map::new();
    let mut primary = serde_json::Map::new();
    let mut agents = serde_json::Map::new();
    for provider in ["claude", "codex"] {
        let name = format!("test-{provider}");
        profiles.insert(name.clone(), json!({"provider":provider,"home":root.join(format!("home/.adk/profiles/{provider}/{name}")),"env":{}}));
        primary.insert(provider.into(), json!(name));
        agents.insert(
            provider.into(),
            json!({"display_name":provider,"provider":provider,"auth_profile":name}),
        );
    }
    write(
        &root.join("config/org.yaml"),
        &json!({"version":1,"agents":agents,"provider_auth_profiles":profiles,"provider_auth_primary_profiles":primary}),
    );
    (temp, root)
}

pub(crate) fn command(root: &Path) -> Command {
    let manifest = read_json(&root.join("manifest.json")).unwrap();
    let config = read_json(&root.join("config/agentdesk.yaml")).unwrap();
    let env = environment::Environment::load(root, &manifest, &config).unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.env_clear().envs(&env.required).envs(&env.flags);
    command
}

fn write(path: &Path, value: &Value) {
    std::fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
}

#[test]
fn host_scope_rejects_foreign_receipt_socket_and_node() {
    let (_temp, root) = fixture();
    let manifest_path = root.join("manifest.json");
    let config_path = root.join("config/agentdesk.yaml");
    let scope = load_instance_declaration(&manifest_path, &root, &config_path).unwrap();
    assert_eq!(scope.tmux.socket, root.join("runtime/tmux/socket"));
    assert!(scope.herdr.home.starts_with(&root));
    let original = read_json(&manifest_path).unwrap();
    for relative in ["config/renamed.yaml", "agentdesk.yaml"] {
        let candidate = root.join(relative);
        write(&candidate, &read_json(&config_path).unwrap());
        let mut changed = original.clone();
        changed["config_path"] = json!(candidate);
        write(&manifest_path, &changed);
        assert!(load_instance_declaration(&manifest_path, &root, &candidate).is_err());
    }
    write(&manifest_path, &original);
    for (relative, key, value) in [
        (
            "receipt.json",
            "/instance_id",
            json!(uuid::Uuid::new_v4().to_string()),
        ),
        (
            "manifest.json",
            "/hosts/tmux/socket",
            json!("/synthetic/release/socket"),
        ),
        ("manifest.json", "/execution_node_id", json!("test-foreign")),
        (
            "config/agentdesk.yaml",
            "/cluster/instance_id",
            json!("foreign"),
        ),
    ] {
        let path = root.join(relative);
        let original = read_json(&path).unwrap();
        let mut changed = original.clone();
        *changed.pointer_mut(key).unwrap() = value;
        write(&path, &changed);
        assert!(
            load_instance_declaration(&manifest_path, &root, &config_path).is_err(),
            "{relative}:{key}"
        );
        write(&path, &original);
    }
}

#[cfg(unix)]
#[test]
fn host_scope_rejects_linked_manifest_and_socket_ancestor() {
    use std::os::unix::fs::symlink;
    let (_temp, root) = fixture();
    let manifest = root.join("manifest.json");
    let config = root.join("config/agentdesk.yaml");
    let real = root.join("declaration.json");
    std::fs::rename(&manifest, &real).unwrap();
    symlink(&real, &manifest).unwrap();
    assert!(load_instance_declaration(&manifest, &root, &config).is_err());
    std::fs::remove_file(&manifest).unwrap();
    std::fs::rename(&real, &manifest).unwrap();
    let socket_dir = root.join("runtime/tmux");
    std::fs::remove_dir(&socket_dir).unwrap();
    symlink(root.join("runtime/herdr"), &socket_dir).unwrap();
    assert!(load_instance_declaration(&manifest, &root, &config).is_err());
}

#[test]
fn test_instance_herdr_accepts_only_its_dedicated_endpoint() {
    let id = "c48f4954-32b9-4d48-b2eb-bcf7eb2a35a1";
    let host = HerdrHost {
        socket: "/instance/runtime/herdr/socket".into(),
        home: "/instance/herdr-home".into(),
        session: format!("adk-test-{id}"),
        endpoint_key: format!("test-{id}"),
    };
    let mut config = super::super::Config::default();
    config.cluster.instance_id = Some(id.into());
    assert!(validate_hosts(&config, id, &host).is_ok());
    let endpoint = super::super::session_hosts::EndpointConfig {
        mode: super::super::session_hosts::EndpointMode::LegacyDedicated,
        execution_node: id.into(),
        socket_path: host.socket.clone(),
        herdr_home: host.home.clone(),
        herdr_session: host.session.clone(),
    };
    config
        .session_hosts
        .herdr
        .endpoints
        .insert(host.endpoint_key.clone(), endpoint.clone());
    config
        .session_hosts
        .herdr
        .channels
        .insert("41".into(), host.endpoint_key.clone());
    assert!(validate_hosts(&config, id, &host).is_ok());
    for variant in [
        super::super::session_hosts::EndpointConfig {
            mode: super::super::session_hosts::EndpointMode::UserDefault,
            ..endpoint.clone()
        },
        super::super::session_hosts::EndpointConfig {
            execution_node: "release".into(),
            ..endpoint.clone()
        },
        super::super::session_hosts::EndpointConfig {
            socket_path: "/release/herdr.sock".into(),
            ..endpoint.clone()
        },
        super::super::session_hosts::EndpointConfig {
            herdr_home: "/release/herdr-home".into(),
            ..endpoint.clone()
        },
        super::super::session_hosts::EndpointConfig {
            herdr_session: "default".into(),
            ..endpoint.clone()
        },
        super::super::session_hosts::EndpointConfig {
            socket_path: Default::default(),
            ..endpoint.clone()
        },
    ] {
        config
            .session_hosts
            .herdr
            .endpoints
            .insert(host.endpoint_key.clone(), variant);
        assert!(validate_hosts(&config, id, &host).is_err());
    }
    config.session_hosts.herdr.endpoints.clear();
    config
        .session_hosts
        .herdr
        .endpoints
        .insert("release".into(), endpoint);
    assert!(validate_hosts(&config, id, &host).is_err());
    config.session_hosts.herdr.endpoints.clear();
    config.session_hosts.herdr.channels.clear();
    config.cluster.instance_id = Some(format!("test-{id}"));
    assert!(validate_hosts(&config, id, &host).is_err());
}

#[cfg(unix)]
#[test]
fn initialized_test_scope_names_sessions_and_refuses_herdr_runtime() {
    use crate::services::herdr_launch::{
        HerdrCreateOutcome, HerdrCreateRequest, HerdrLaunchEndpoint, HerdrLaunchHost,
        unplaced::Residue,
    };
    use crate::services::session_host::{
        HerdrEndpoint, RestoreResume, ServerWitness, SocketHerdrLaunchHost, autostart,
    };
    use std::os::unix::net::UnixListener;

    let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
    if std::env::var_os(MARKER).is_none() {
        let (_temp, root) = fixture();
        let output = command(&root)
            .args([
                "--exact",
                concat!(
                    module_path!(),
                    "::initialized_test_scope_names_sessions_and_refuses_herdr_runtime"
                )
                .strip_prefix("agentdesk::")
                .unwrap(),
                "--nocapture",
            ])
            .env(MARKER, root.join("manifest.json"))
            .env("AGENTDESK_ROOT_DIR", &root)
            .env("AGENTDESK_CONFIG", root.join("config/agentdesk.yaml"))
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(stdout.contains("1 passed; 0 failed; 0 ignored"), "{stdout}");
        return;
    }
    initialize().unwrap();
    let instance = current().unwrap().unwrap();
    let _marker = crate::config::TestEnvVarGuard::remove_after_shared_test_env_lock(MARKER);
    assert!(requested());
    assert_eq!(
        current().unwrap().unwrap().instance_id,
        instance.instance_id
    );
    let provider = crate::services::provider::ProviderKind::Claude;
    let parse = crate::services::provider::parse_provider_and_channel_from_tmux_name;
    let name = provider.build_tmux_session_name("lane-t41");
    assert_eq!(name, format!("{}-claude-lane-t41", instance.tmux.prefix));
    assert_eq!(parse(&name), Some((provider, "lane-t41".into())));
    assert!(parse("AgentDesk-claude-lane-t41").is_none());
    assert!(parse("AdkTest-633db1b8-a92b-4169-a22a-23e9173dbeaa-claude-lane-t41").is_none());

    let scratch = tempfile::Builder::new()
        .prefix("adk-i2a-")
        .tempdir_in("/tmp")
        .unwrap();
    let socket = scratch.path().join("foreign.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = HerdrEndpoint::new("foreign", "release", &socket, "default")
        .unwrap()
        .with_herdr_home(scratch.path())
        .unwrap();
    let host = SocketHerdrLaunchHost::new(vec![endpoint], Default::default());
    let launch = HerdrLaunchEndpoint {
        execution_node: "foreign".into(),
        config_key: "release".into(),
        socket_addr: socket.display().to_string(),
        herdr_session: "default".into(),
    };
    let witness = ServerWitness::for_test(42);
    assert!(matches!(
        host.restore_resume(&launch),
        RestoreResume::Unverified(_)
    ));
    let request = HerdrCreateRequest {
        endpoint: launch.clone(),
        label: "foreign-workspace".into(),
        cwd: scratch.path().into(),
        command: "bash /run/launch.sh".into(),
        restore_off_witness: witness.clone(),
    };
    assert_eq!(
        host.create(&request),
        HerdrCreateOutcome::NotSent("test instance host declaration rejected".into())
    );
    assert_eq!(
        host.channel_panes(&launch, "foreign-workspace", &witness),
        Err("test Herdr workspace adoption is not admitted".into())
    );
    let residue = Residue {
        pane_id: "foreign-pane".into(),
        label: "foreign-workspace".into(),
        root: crate::db::dispatched_sessions::hosted_execution::ProcessStamp {
            pid: 42,
            start: "darwin:1000.0".into(),
        },
        witness,
    };
    assert_eq!(
        host.dispose_residue(&launch, "foreign-nonce", &residue),
        Err("test Herdr residue disposal is not admitted".into())
    );
    autostart::spawn::demand(&launch);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    assert_eq!(
        runtime.block_on(autostart::spawn::ensure(&launch)),
        autostart::spawn::Ensured::Refused("test_instance_manual_daemon_required")
    );
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    exercise_owner_receipt(instance);
}

#[cfg(unix)]
fn exercise_owner_receipt(instance: &'static TestInstance) {
    use crate::services::herdr_launch::HerdrLaunchEndpoint;
    use crate::services::session_host::ServerWitness;
    use crate::utils::secret_file::private_directory::PrivateDirectory;
    let directory = PrivateDirectory::open(&instance.root.join("runtime")).unwrap();
    let socket = &instance.herdr.socket;
    assert!(owned_herdr(socket).is_err());
    assert!(owned_herdr(Path::new("/synthetic/release/socket")).is_err());
    let mut value = json!({"schema_version":1,"instance_id":instance.instance_id,
        "endpoint":instance.herdr,"binary_sha256":"a".repeat(64),
        "daemon":{"pid":42,"start_identity":{"platform":"darwin","seconds":1000,"micros":0}},
        "workspaces":[]});
    let publish = |value: &Value| {
        directory
            .write_atomic("herdr-owner.json", &serde_json::to_vec(value).unwrap())
            .unwrap()
    };
    for (pointer, replacement) in [
        ("/schema_version", json!(2)),
        ("/instance_id", json!("foreign")),
        ("/daemon/pid", json!(0)),
        ("/daemon/start_identity/micros", json!(1_000_000)),
        ("/binary_sha256", json!("not-a-hash")),
        (
            "/workspaces",
            json!([{"workspace_id":"w1","pane_id":"w1:p1"},{"workspace_id":"w1","pane_id":"w1:p2"}]),
        ),
    ] {
        let mut foreign = value.clone();
        *foreign.pointer_mut(pointer).unwrap() = replacement;
        publish(&foreign);
        assert!(owned_herdr(socket).is_err(), "{pointer}");
    }
    publish(&value);
    let receipt = owned_herdr(socket).unwrap().unwrap();
    let mut witness = ServerWitness::for_test(42);
    witness.socket = socket.clone();
    assert!(receipt.matches_witness(&witness));
    assert!(!receipt.contains_pane("w1:p1"));
    let endpoint = HerdrLaunchEndpoint {
        execution_node: instance.instance_id.clone(),
        config_key: instance.herdr.endpoint_key.clone(),
        socket_addr: socket.display().to_string(),
        herdr_session: instance.herdr.session.clone(),
    };
    let label = format!("{}-claude-test#{}", instance.tmux.prefix, "a".repeat(32));
    let mut guard = begin_herdr_create(&endpoint, &witness, &label, &instance.root)
        .unwrap()
        .unwrap();
    assert!(begin_herdr_create(&endpoint, &witness, &label, &instance.root).is_err());
    assert!(Command::new("python3").args(["-c", "import fcntl,sys\nf=open(sys.argv[1],'r+b')\ntry: fcntl.flock(f,fcntl.LOCK_EX|fcntl.LOCK_NB)\nexcept BlockingIOError: sys.exit(0)\nsys.exit(1)"])
        .arg(instance.root.join("runtime/herdr-owner.lock"))
        .status().unwrap().success());
    assert!(
        owned_herdr(socket)
            .unwrap()
            .unwrap()
            .matches_witness(&witness)
    );
    guard.record_created("w1", "w1:p1").unwrap();
    assert!(owned_herdr(socket).unwrap().unwrap().contains_pane("w1:p1"));
    assert!(guard.record_created("w1", "w1:p1").is_err());
    value["binary_sha256"] = json!("b".repeat(64));
    publish(&value);
    assert!(guard.record_created("w2", "w2:p1").is_err());
    assert_eq!(
        read_json(&instance.root.join("runtime/herdr-owner.json")).unwrap(),
        value
    );
}
