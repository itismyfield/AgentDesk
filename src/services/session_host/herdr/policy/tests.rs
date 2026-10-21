use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, SystemTime};

use super::*;
use crate::services::session_host::herdr::contract::ServerWitness;

const FALSE: &str = "[session]\nresume_agents_on_restore = false\n";

struct Source(Mutex<peer::PeerSnapshot>);
impl PeerSource for Source {
    fn snapshot(&self) -> Result<peer::PeerSnapshot, Unverified> {
        Ok(self.0.lock().unwrap().clone())
    }
}

struct CountChecker {
    calls: AtomicUsize,
    result: Result<(), Unverified>,
}
impl Default for CountChecker {
    fn default() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            result: Ok(()),
        }
    }
}
#[async_trait::async_trait]
impl ConfigChecker for CountChecker {
    async fn check(&self, _: &peer::PeerSnapshot) -> Result<(), Unverified> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.result
    }
}

fn fixture() -> (tempfile::TempDir, Source) {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    fs::write(&config, FALSE).unwrap();
    let checker = dir.path().join("checker");
    fs::write(&checker, "#!/bin/sh\nprintf 'config: ok\\n'\n").unwrap();
    fs::set_permissions(&checker, fs::Permissions::from_mode(0o700)).unwrap();
    let source = Source(Mutex::new(peer::PeerSnapshot {
        witness: ServerWitness::for_test(1),
        started: SystemTime::now() + Duration::from_secs(60),
        home: dir.path().into(),
        config,
        image: file::image(&checker).unwrap(),
        checker,
    }));
    (dir, source)
}

#[tokio::test]
async fn semantic_false_and_whole_checker_are_both_required() {
    for text in [
        FALSE,
        "session.resume_agents_on_restore=false\n",
        "session={resume_agents_on_restore=false}\n",
    ] {
        let (_dir, source) = fixture();
        fs::write(&source.snapshot().unwrap().config, text).unwrap();
        let checker = CountChecker::default();
        assert!(
            ProofCache::default()
                .next_restart(&source, &checker)
                .await
                .is_ok(),
            "semantic boolean false must pass: {text}"
        );
        assert_eq!(
            checker.calls.load(Ordering::SeqCst),
            1,
            "whole checker must run"
        );
    }
    for (text, reason) in [
        (
            "[session]\nresume_agents_on_restore='false'",
            Unverified::ConfigInvalid,
        ),
        (
            "[session]\nresume_agents_on_restore=true",
            Unverified::ResumeEnabled,
        ),
        (
            "[session]\nresume_agents_on_restore=false\nresume_agents_on_restore=false",
            Unverified::ConfigInvalid,
        ),
        (
            "[session]\nresume_agents_on_restore=false\n[invalid",
            Unverified::ConfigInvalid,
        ),
        ("[theme]\ncolor='blue'", Unverified::ConfigInvalid),
    ] {
        let (_dir, source) = fixture();
        fs::write(&source.snapshot().unwrap().config, text).unwrap();
        assert_eq!(
            ProofCache::default()
                .next_restart(&source, &CountChecker::default())
                .await,
            Err(reason),
            "semantic policy refusal: {text}"
        );
    }
    let (_dir, source) = fixture();
    fs::write(
        &source.snapshot().unwrap().config,
        format!("{FALSE}unknown=true\n"),
    )
    .unwrap();
    let checker = CountChecker {
        result: Err(Unverified::Diagnostics),
        ..CountChecker::default()
    };
    assert_eq!(
        ProofCache::default().next_restart(&source, &checker).await,
        Err(Unverified::Diagnostics),
        "whole-config diagnostics must refuse even with false key"
    );
}

#[tokio::test]
async fn current_cache_rechecks_edits_but_preserves_historical_startup() {
    let (_dir, source) = fixture();
    let checker = CountChecker::default();
    let cache = ProofCache::default();
    let first = cache.next_restart(&source, &checker).await.unwrap();
    let witness = first.witness().clone();
    let startup = cache.startup_for(&witness).unwrap().unwrap();
    assert_eq!(startup.witness(), &witness);
    cache.next_restart(&source, &checker).await.unwrap();
    assert_eq!(
        checker.calls.load(Ordering::SeqCst),
        1,
        "unchanged evidence reuses checker result"
    );
    let config = source.snapshot().unwrap().config;
    fs::write(&config, format!("{FALSE}[theme]\nname='blue'\n")).unwrap();
    cache.next_restart(&source, &checker).await.unwrap();
    assert_eq!(
        checker.calls.load(Ordering::SeqCst),
        2,
        "changed config needs full recheck"
    );
    assert_eq!(
        cache.startup_for(&witness).unwrap(),
        Some(startup.clone()),
        "theme edits retain startup evidence"
    );
    fs::write(&config, "[session]\nresume_agents_on_restore=true\n").unwrap();
    assert_eq!(
        cache.next_restart(&source, &checker).await,
        Err(Unverified::ResumeEnabled),
        "current true cannot reuse cached false"
    );
    fs::write(&config, "invalid[").unwrap();
    assert_eq!(
        cache.next_restart(&source, &checker).await,
        Err(Unverified::ConfigInvalid)
    );
    assert_eq!(
        cache.startup_for(&witness).unwrap(),
        Some(startup),
        "invalid current config retains past proof"
    );
    assert_eq!(
        first.revalidate(&source),
        Err(Unverified::Changed),
        "old create token fences edits"
    );
}

#[tokio::test]
async fn cache_keys_include_checker_and_exact_server_witness() {
    let (_dir, source) = fixture();
    let checker = CountChecker::default();
    let cache = ProofCache::default();
    let old = cache.next_restart(&source, &checker).await.unwrap();
    {
        let mut peer = source.0.lock().unwrap();
        let replacement = peer.checker.with_extension("new");
        fs::write(
            &replacement,
            "#!/bin/sh\nprintf 'config: ok\\n'\n# replacement\n",
        )
        .unwrap();
        fs::rename(replacement, &peer.checker).unwrap();
        peer.image = file::image(&peer.checker).unwrap();
    }
    cache.next_restart(&source, &checker).await.unwrap();
    assert_eq!(
        checker.calls.load(Ordering::SeqCst),
        2,
        "new checker image needs full recheck"
    );
    source.0.lock().unwrap().witness.pid += 1;
    let new = cache.next_restart(&source, &checker).await.unwrap();
    assert_eq!(
        checker.calls.load(Ordering::SeqCst),
        3,
        "new peer needs full recheck"
    );
    assert_ne!(old.witness(), new.witness());
    assert_eq!(
        old.revalidate(&source),
        Err(Unverified::Changed),
        "old server token must not authorize replacement"
    );
    assert!(
        cache
            .startup_for(&ServerWitness::for_test(999))
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn fresh_false_can_have_no_startup_proof() {
    let (_dir, source) = fixture();
    source.0.lock().unwrap().started = SystemTime::UNIX_EPOCH;
    let cache = ProofCache::default();
    let next = cache
        .next_restart(&source, &CountChecker::default())
        .await
        .unwrap();
    assert!(
        cache.startup_for(next.witness()).unwrap().is_none(),
        "next restart evidence does not invent historical startup"
    );
    next.revalidate(&source).unwrap();
}

struct ChangedDuringCheck<'a>(&'a Source);
#[async_trait::async_trait]
impl ConfigChecker for ChangedDuringCheck<'_> {
    async fn check(&self, _: &peer::PeerSnapshot) -> Result<(), Unverified> {
        self.0.0.lock().unwrap().witness.pid += 1;
        Ok(())
    }
}

#[tokio::test]
async fn checker_cannot_span_two_server_witnesses() {
    let (_dir, source) = fixture();
    let cache = ProofCache::default();
    assert_eq!(
        cache
            .next_restart(&source, &ChangedDuringCheck(&source))
            .await,
        Err(Unverified::Changed),
        "peer change during checker must refuse"
    );
    assert!(
        cache
            .startup_for(&ServerWitness::for_test(1))
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn corrupt_cache_and_checker_failure_return_refusals() {
    let (_dir, source) = fixture();
    let cache = ProofCache::default();
    assert!(std::panic::catch_unwind(|| cache.poison_for_test()).is_err());
    assert_eq!(
        cache.next_restart(&source, &CountChecker::default()).await,
        Err(Unverified::Cache),
        "cache failure must return typed refusal"
    );
    assert_eq!(
        cache.startup_for(&source.snapshot().unwrap().witness),
        Err(Unverified::Cache)
    );
    for reason in [Unverified::Timeout, Unverified::Checker, Unverified::Peer] {
        let checker = CountChecker {
            result: Err(reason),
            ..CountChecker::default()
        };
        assert_eq!(
            ProofCache::default().next_restart(&source, &checker).await,
            Err(reason)
        );
    }
}

#[tokio::test]
async fn local_checker_is_usable_by_proof_builder() {
    let (_dir, source) = fixture();
    assert!(
        ProofCache::default()
            .next_restart(&source, &LocalChecker)
            .await
            .is_ok(),
        "bounded local checker supplies the full-config proof"
    );
}
