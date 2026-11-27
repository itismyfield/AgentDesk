//! Fresh-process config snapshot fixture the upper-crate snapshot reader tests share.

use super::*;

pub fn isolated(body: impl FnOnce()) {
    const CHILD: &str = "ADK_TEST_SNAPSHOT_READER_CHILD";
    let thread = std::thread::current();
    let name = thread.name().expect("named libtest worker");
    if std::env::var(CHILD).ok().as_deref() == Some(name) {
        assert!(
            snapshot().is_none(),
            "each reader starts in a fresh process"
        );
        body();
        return;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env(CHILD, name)
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{name}: {stdout}\n{stderr}");
    assert!(stdout.contains("1 passed; 0 failed; 0 ignored"), "{stdout}");
    println!("snapshot child executed: {name}");
}

pub struct Fixture {
    _root_env: crate::config::TestEnvVarGuard,
    root: tempfile::TempDir,
    pub path: PathBuf,
}

impl Fixture {
    pub fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let root_env = crate::config::set_agentdesk_root_for_test(root.path());
        let path = root.path().join("config/agentdesk.yaml");
        std::env::set_current_dir(root.path()).unwrap();
        // Each fixture runs in a fresh single-test child, before its runtime starts.
        unsafe {
            std::env::set_var("AGENTDESK_CONFIG", &path);
            std::env::remove_var("DATABASE_URL");
        }
        Self {
            _root_env: root_env,
            root,
            path,
        }
    }

    pub fn root(&self) -> &Path {
        self.root.path()
    }

    pub(crate) fn write_at(&self, path: &Path, yaml: &str) {
        let mut value: serde_yaml::Value = serde_yaml::from_str(yaml).unwrap();
        let map = value.as_mapping_mut().unwrap();
        map.entry("server".into())
            .or_insert_with(|| serde_yaml::Value::Mapping(Default::default()));
        map.entry("data".into()).or_insert_with(|| {
            serde_yaml::to_value(serde_json::json!({"dir": self.root().join("data")})).unwrap()
        });
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_yaml::to_string(&value).unwrap()).unwrap();
    }

    pub fn write(&self, yaml: &str) {
        self.write_at(&self.path, yaml);
    }

    pub fn install(&self, initial: &str, disk: &str) {
        self.write(initial);
        crate::config_live_reload::install(load_from_path(&self.path).unwrap());
        self.write(disk);
    }

    pub fn reload(&self) {
        assert!(matches!(
            crate::config_live_reload::reload_from_path(&self.path),
            crate::config_live_reload::ReloadOutcome::Applied { .. }
        ));
    }
}
