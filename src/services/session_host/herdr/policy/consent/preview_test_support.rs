use super::*;
use crate::services::session_host::herdr::contract::ServerWitness;
use crate::services::session_host::herdr::policy::{Unverified, file};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::Mutex;
use std::time::SystemTime;

pub struct Source(pub Mutex<PeerSnapshot>);
impl PeerSource for Source {
    fn snapshot(&self) -> Result<PeerSnapshot, Unverified> {
        Ok(self.0.lock().unwrap().clone())
    }
}
pub fn fixture(body: &str) -> (tempfile::TempDir, Arc<Source>) {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    fs::write(&config, "[session]\nresume_agents_on_restore=true\n").unwrap();
    let checker = dir.path().join("fake-local-checker");
    fs::write(&checker, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(&checker, fs::Permissions::from_mode(0o700)).unwrap();
    let mut witness = ServerWitness::for_test(71);
    witness.socket = dir.path().join("fixture.sock");
    let source = Arc::new(Source(Mutex::new(PeerSnapshot {
        witness,
        socket_dir: file::directory_identity(dir.path()).unwrap(),
        started: SystemTime::now(),
        home: dir.path().into(),
        config,
        image: file::image(&checker).unwrap(),
        checker,
    })));
    (dir, source)
}
