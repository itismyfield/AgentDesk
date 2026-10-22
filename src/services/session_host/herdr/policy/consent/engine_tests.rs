use super::super::super::{Unverified, file, peer::PeerSnapshot};
use super::super::preview;
use super::super::settlement::SettlementKey;
use super::super::{IntentRef, Preview};
use super::*;
use crate::services::session_host::herdr::contract::ServerWitness;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::Mutex;
use std::time::SystemTime;

struct Source(PeerSnapshot);
impl PeerSource for Source {
    fn snapshot(&self) -> Result<PeerSnapshot, Unverified> {
        Ok(self.0.clone())
    }
}

fn current(consent: &Consent) -> Current {
    Current {
        owner: Some(consent.owner),
        bot: consent.bot,
        provider: consent.provider.clone(),
        channel: consent.channel,
        message: consent.message,
        nonce: consent.nonce.clone(),
        key: consent.key.clone(),
        preview: consent.preview.clone(),
        local_node: consent.key.node.clone(),
    }
}

#[derive(Default)]
struct FakeOwner {
    fail: bool,
    fail_after: Option<usize>,
    calls: usize,
    local_node: Option<String>,
}
#[async_trait::async_trait]
impl Owner for FakeOwner {
    async fn current(&mut self, consent: &Consent) -> Result<Current, String> {
        self.calls += 1;
        if self.fail || self.fail_after.is_some_and(|limit| self.calls > limit) {
            Err("current owner unavailable".into())
        } else {
            let mut current = current(consent);
            if let Some(local_node) = &self.local_node {
                current.local_node = local_node.clone();
            }
            Ok(current)
        }
    }
}

#[derive(Default)]
struct FakeCards {
    sends: Vec<card::Card>,
    replacements: Vec<card::Card>,
    disabled: Vec<u64>,
}
#[async_trait::async_trait]
impl Cards for FakeCards {
    async fn send(&mut self, card: &card::Card) -> Result<u64, String> {
        self.sends.push(card.clone());
        Ok(40)
    }
    async fn replace(&mut self, _: u64, card: &card::Card) -> Result<(), String> {
        self.replacements.push(card.clone());
        Ok(())
    }
    async fn disable(&mut self, message: u64) -> Result<(), String> {
        self.disabled.push(message);
        Ok(())
    }
}

struct FakeCustody {
    store: ConsentStore,
    released: Vec<SettlementKey>,
    woken: Vec<SettlementKey>,
    events: Arc<Mutex<Vec<&'static str>>>,
}
impl CustodySink for FakeCustody {
    fn release(&mut self, key: &SettlementKey, _: &IntentRef) -> Result<(), String> {
        let receipt = self
            .store
            .load_all()?
            .into_iter()
            .find(|c| c.nonce == key.nonce)
            .ok_or("receipt missing")?;
        assert_eq!(
            receipt.state,
            State::Applied,
            "engine_receipt_before_release_contract"
        );
        assert!(
            receipt.apply.as_ref().and_then(|r| r.after).is_some(),
            "engine_receipt_before_release_contract"
        );
        if !self.released.contains(key) {
            self.released.push(key.clone());
            self.events.lock().unwrap().push("release");
        }
        Ok(())
    }
    fn wake(&mut self, key: &SettlementKey, _: &IntentRef) -> Result<(), String> {
        if !self.woken.contains(key) {
            self.woken.push(key.clone());
            self.events.lock().unwrap().push("wake");
        }
        Ok(())
    }
}

struct Rig {
    _root: tempfile::TempDir,
    store: ConsentStore,
    source: Arc<Source>,
    owner: FakeOwner,
    cards: FakeCards,
    custody: FakeCustody,
    checker_baseline: usize,
}
impl Rig {
    async fn new(diagnostics: bool, fail_receipt: bool) -> (Self, Prepared) {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config.toml");
        fs::write(&config, "[session]\nresume_agents_on_restore = true\n").unwrap();
        let mut witness = ServerWitness::for_test(71);
        witness.socket = root.path().join("socket");
        let key = ConsentKey {
            node: "execution-node".into(),
            user: 501,
            socket: witness.socket.clone(),
            named_session: "agentdesk".into(),
            config: config.clone(),
            target: config.canonicalize().unwrap(),
        };
        let store_path = root.path().join("consents");
        let marker = store_path.join(format!(
            "{}.pending",
            super::super::super::durable::key(&key).unwrap()
        ));
        let checker = root.path().join("fake-checker");
        let mut body = "#!/bin/sh\nphase=preview\nif [ -f \"$HOME/engine-ready\" ]; then\n  case \"$HERDR_CONFIG_PATH\" in \"$HOME/config.toml\") phase=final;; *) phase=candidate;; esac\nfi\nprintf '%s\\n' \"$phase\" >> \"$HOME/checker.calls\"\n".to_owned();
        if fail_receipt {
            body.push_str(&format!(
                "if [ -f \"$HOME/engine-ready\" ]; then printf pending > '{}'; fi\n",
                marker.display()
            ));
        }
        body.push_str(if diagnostics { "if [ -f \"$HOME/engine-ready\" ]; then printf 'using defaults\\n'; else printf 'config: ok\\n'; fi\n" } else { "printf 'config: ok\\n'\n" });
        fs::write(&checker, body).unwrap();
        fs::set_permissions(&checker, fs::Permissions::from_mode(0o700)).unwrap();
        let source = Arc::new(Source(PeerSnapshot {
            witness,
            socket_dir: file::directory_identity(root.path()).unwrap(),
            started: SystemTime::now(),
            home: root.path().into(),
            config,
            checker: checker.clone(),
            image: file::image(&checker).unwrap(),
        }));
        let prepared = preview::prepare(source.clone(), 1).await.unwrap();
        let checker_baseline = fs::read_to_string(source.0.home.join("checker.calls"))
            .unwrap_or_default()
            .lines()
            .count();
        fs::write(source.0.home.join("engine-ready"), "ready").unwrap();
        let store = ConsentStore::new(Some(store_path.clone()));
        let custody = FakeCustody {
            store: ConsentStore::new(Some(store_path)),
            released: Vec::new(),
            woken: Vec::new(),
            events: Arc::new(Mutex::new(Vec::new())),
        };
        (
            Self {
                _root: root,
                store,
                source,
                owner: FakeOwner::default(),
                cards: FakeCards::default(),
                custody,
                checker_baseline,
            },
            prepared,
        )
    }
    fn question(&self, preview: &Preview) -> Question {
        Question {
            key: ConsentKey {
                node: "execution-node".into(),
                user: 501,
                socket: self.source.0.witness.socket.clone(),
                named_session: "agentdesk".into(),
                config: self.source.0.config.clone(),
                target: self.source.0.config.canonicalize().unwrap(),
            },
            preview: preview.clone(),
            owner: 1,
            bot: 2,
            provider: "claude".into(),
            channel: 3,
            message: 0,
            intent: IntentRef {
                store: "queue".into(),
                id: "original".into(),
                provider: "claude".into(),
                channel: 3,
                source_message: 9,
                fifo: 10,
            },
        }
    }
    async fn handle(
        &mut self,
        event: Event,
        prepared: Option<Prepared>,
    ) -> Result<Option<Transition>, String> {
        Engine {
            store: &self.store,
            source: self.source.clone(),
            owner: &mut self.owner,
            cards: &mut self.cards,
            custody: &mut self.custody,
        }
        .handle(event, prepared)
        .await
    }
    async fn bound(&mut self, preview: &Preview) -> Consent {
        self.handle(
            Event::Hold {
                question: Box::new(self.question(preview)),
                now: 100,
                request: Request::ExplicitLaunch,
            },
            None,
        )
        .await
        .unwrap()
        .unwrap()
        .consent
    }
    fn decide(consent: &Consent, yes: bool) -> Event {
        Event::Decide {
            expected: Box::new(consent.clone()),
            current: Box::new(current(consent)),
            actor: consent.owner,
            yes,
            now: 101,
        }
    }
    fn calls(&self) -> usize {
        fs::read_to_string(self.source.0.home.join("checker.calls"))
            .unwrap_or_default()
            .lines()
            .count()
            - self.checker_baseline
    }
}

#[tokio::test]
async fn engine_sends_disabled_then_binds_before_enabling_and_never_resends() {
    let (mut rig, prepared) = Rig::new(false, false).await;
    let held = rig
        .handle(
            Event::Hold {
                question: Box::new(rig.question(&prepared.token)),
                now: 1,
                request: Request::Automatic,
            },
            None,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(held.effect, Effect::KeepHeld);
    assert!(
        rig.cards.sends.is_empty(),
        "engine_automatic_card_zero_contract"
    );
    let bound = rig.bound(&prepared.token).await;
    assert_eq!(rig.cards.sends.len(), 1);
    assert!(
        rig.cards.sends[0].buttons.iter().all(|b| b.disabled),
        "engine_send_before_bind_disabled_contract"
    );
    assert!(
        rig.cards.replacements[0]
            .buttons
            .iter()
            .all(|b| !b.disabled)
    );
    assert_ne!(bound.message, 0);
    rig.handle(Event::Recover(bound.key.clone()), None)
        .await
        .unwrap();
    assert_eq!(
        rig.cards.sends.len(),
        1,
        "engine_restart_no_resend_contract"
    );
    assert_eq!(rig.calls(), 0);
    assert!(rig.custody.released.is_empty());
}

#[tokio::test]
async fn engine_failed_bind_disables_the_sent_card_without_releasing_or_resending() {
    let (mut rig, prepared) = Rig::new(false, false).await;
    rig.owner.fail = true;
    let question = rig.question(&prepared.token);
    let key = question.key.clone();
    assert!(
        rig.handle(
            Event::Hold {
                question: Box::new(question),
                now: 100,
                request: Request::ExplicitLaunch
            },
            None
        )
        .await
        .is_err()
    );
    assert_eq!(
        rig.cards.disabled,
        vec![40],
        "engine_failed_bind_disables_card_contract"
    );
    let reserved = rig.store.load(&key).unwrap().unwrap();
    assert_eq!(reserved.message, 0);
    rig.handle(Event::Recover(key), None).await.unwrap();
    assert_eq!(rig.cards.sends.len(), 1);
    assert_eq!(rig.calls(), 0);
    assert!(rig.custody.released.is_empty());
}

#[tokio::test]
async fn engine_full_checker_apply_receipt_then_release_and_recovery_are_idempotent() {
    let (mut rig, prepared) = Rig::new(false, false).await;
    let bound = rig.bound(&prepared.token).await;
    let applied = rig
        .handle(Rig::decide(&bound, true), Some(prepared))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        applied.consent.state,
        State::Applied,
        "engine_verified_apply_contract"
    );
    assert_eq!(
        &*rig.custody.events.lock().unwrap(),
        &["release", "wake"],
        "engine_receipt_release_wake_order_contract"
    );
    assert_eq!(rig.calls(), 2, "engine_full_checker_required_contract");
    assert_eq!(
        fs::read_to_string(rig.source.0.home.join("checker.calls")).unwrap(),
        "preview\ncandidate\nfinal\n",
        "engine_full_checker_phase_order_contract"
    );
    assert!(
        fs::read_to_string(&rig.source.0.config)
            .unwrap()
            .contains("false")
    );
    for _ in 0..2 {
        rig.handle(Event::Recover(bound.key.clone()), None)
            .await
            .unwrap();
    }
    rig.handle(Rig::decide(&applied.consent, true), None)
        .await
        .unwrap();
    assert_eq!(
        rig.custody.released.len(),
        1,
        "engine_recovery_release_once_contract"
    );
    assert_eq!(rig.custody.woken.len(), 1);
    assert_eq!(rig.calls(), 2, "engine_duplicate_apply_zero_contract");
}

#[tokio::test]
async fn engine_receipt_failure_after_publication_keeps_all_custody() {
    let (mut rig, prepared) = Rig::new(false, true).await;
    let bound = rig.bound(&prepared.token).await;
    let result = rig.handle(Rig::decide(&bound, true), Some(prepared)).await;
    assert!(
        result.is_err(),
        "engine_receipt_failure_is_not_success_contract"
    );
    assert!(
        fs::read_to_string(&rig.source.0.config)
            .unwrap()
            .contains("false")
    );
    assert_eq!(rig.calls(), 2);
    assert!(
        rig.custody.released.is_empty(),
        "engine_receipt_failure_release_zero_contract"
    );
    assert!(rig.custody.woken.is_empty());
}

#[tokio::test]
async fn engine_stale_preview_and_changed_owner_never_run_the_checker_or_release() {
    for (changed_owner, missing_preview) in [(false, false), (true, false), (false, true)] {
        let (mut rig, mut prepared) = Rig::new(false, false).await;
        let bound = rig.bound(&prepared.token).await;
        if changed_owner {
            rig.owner.fail = true;
        } else if !missing_preview {
            prepared.token.revision += 1;
        }
        let prepared = (!missing_preview).then_some(prepared);
        assert!(
            rig.handle(Rig::decide(&bound, true), prepared)
                .await
                .is_err(),
            "engine_current_preview_owner_contract"
        );
        assert_eq!(
            rig.store.load(&bound.key).unwrap().unwrap().state,
            State::Pending,
            "engine_preflight_failure_keeps_retryable_pending_contract"
        );
        assert_eq!(
            rig.calls(),
            0,
            "engine_rejected_preview_checker_zero_contract"
        );
        assert!(rig.custody.released.is_empty());
        assert!(
            fs::read_to_string(&rig.source.0.config)
                .unwrap()
                .contains("true")
        );
        rig.owner.fail = false;
        let retry = Event::Retry {
            question: Box::new(rig.question(&bound.preview)),
            actor: 1,
            owner: Some(1),
            local_node: "execution-node".into(),
            now: 102,
        };
        assert_eq!(
            rig.handle(retry, None).await.unwrap().unwrap().effect,
            Effect::EnableButtons,
            "engine_preflight_failure_allows_retry_contract"
        );
    }
}

#[tokio::test]
async fn engine_checker_failure_is_recorded_before_result_and_never_releases() {
    let (mut rig, prepared) = Rig::new(true, false).await;
    let bound = rig.bound(&prepared.token).await;
    let failed = rig
        .handle(Rig::decide(&bound, true), Some(prepared))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        failed.consent.state,
        State::ApplyFailed,
        "engine_diagnostic_failure_recorded_contract"
    );
    let receipt = rig.store.load(&bound.key).unwrap().unwrap();
    let result = receipt.apply.unwrap();
    assert!(result.after.is_none() && result.backup.is_some());
    assert!(result.detail.unwrap().contains("published=false"));
    assert!(
        rig.custody.released.is_empty(),
        "engine_failed_apply_release_zero_contract"
    );
    assert_eq!(rig.calls(), 1);
}

#[tokio::test]
async fn engine_owner_change_after_acceptance_records_a_retryable_failed_receipt() {
    let (mut rig, prepared) = Rig::new(false, false).await;
    let bound = rig.bound(&prepared.token).await;
    rig.owner.fail_after = Some(rig.owner.calls + 1);
    let failed = rig
        .handle(Rig::decide(&bound, true), Some(prepared))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        failed.consent.state,
        State::ApplyFailed,
        "engine_postaccept_failure_receipt_contract"
    );
    assert!(
        failed
            .consent
            .apply
            .as_ref()
            .unwrap()
            .detail
            .as_ref()
            .unwrap()
            .contains("owner unavailable")
    );
    assert_eq!(rig.calls(), 0);
    assert!(rig.custody.released.is_empty());
    rig.owner.fail_after = None;
    let retried = rig
        .handle(
            Event::Retry {
                question: Box::new(rig.question(&bound.preview)),
                actor: 1,
                owner: Some(1),
                local_node: "execution-node".into(),
                now: 103,
            },
            None,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        retried.effect,
        Effect::EnableButtons,
        "engine_postaccept_failure_retry_contract"
    );
}

#[tokio::test]
async fn engine_no_and_duplicate_decisions_require_the_actual_current_owner() {
    for duplicate in [false, true] {
        let (mut rig, prepared) = Rig::new(false, false).await;
        let bound = rig.bound(&prepared.token).await;
        let expected = if duplicate {
            rig.handle(Rig::decide(&bound, false), None)
                .await
                .unwrap()
                .unwrap()
                .consent
        } else {
            bound
        };
        let snapshot = rig._root.path().join("consents").join(format!(
            "{}.json",
            super::super::super::durable::key(&expected.key).unwrap()
        ));
        let before = fs::read(&snapshot).unwrap();
        let cards_before = (
            rig.cards.sends.len(),
            rig.cards.replacements.len(),
            rig.cards.disabled.len(),
        );
        rig.owner.fail = true;
        assert!(
            rig.handle(Rig::decide(&expected, false), None)
                .await
                .is_err(),
            "engine_every_decision_requires_current_owner_contract"
        );
        assert_eq!(
            fs::read(&snapshot).unwrap(),
            before,
            "engine_stale_owner_decision_changes_nothing_contract"
        );
        assert_eq!(
            (
                rig.cards.sends.len(),
                rig.cards.replacements.len(),
                rig.cards.disabled.len()
            ),
            cards_before
        );
        assert_eq!(rig.calls(), 0);
        assert!(rig.custody.released.is_empty() && rig.custody.woken.is_empty());
    }
}

#[tokio::test]
async fn engine_remote_recovery_never_releases_a_durable_applied_receipt() {
    let (mut rig, prepared) = Rig::new(false, false).await;
    let bound = rig.bound(&prepared.token).await;
    let accepted = Workflow(&rig.store)
        .decide(&bound, &current(&bound), 1, true, 101)
        .unwrap()
        .consent;
    let applied = Workflow(&rig.store)
        .receipt(
            &accepted,
            ApplyResult {
                before: Some([1; 32]),
                after: Some([2; 32]),
                backup: None,
                detail: None,
            },
        )
        .unwrap()
        .consent;
    rig.owner.local_node = Some("other-node".into());
    assert!(
        rig.handle(Event::Recover(bound.key.clone()), None)
            .await
            .is_err(),
        "engine_recovery_requires_execution_node_contract"
    );
    assert_eq!(
        rig.store.load(&bound.key).unwrap().unwrap(),
        applied,
        "engine_remote_recovery_preserves_applied_receipt_contract"
    );
    assert_eq!(rig.store.unresolved().unwrap().len(), 1);
    assert!(
        rig.custody.released.is_empty() && rig.custody.woken.is_empty(),
        "engine_remote_recovery_release_zero_contract"
    );
    rig.owner.local_node = None;
    rig.handle(Event::Recover(bound.key), None).await.unwrap();
    assert_eq!(rig.custody.released.len(), 1);
}

#[tokio::test]
async fn engine_bind_retry_revision_and_missing_recovery_effects_are_interpreted() {
    let (mut rig, prepared) = Rig::new(false, false).await;
    let question = rig.question(&prepared.token);
    let key = question.key.clone();
    assert!(
        rig.handle(Event::Recover(key.clone()), None)
            .await
            .unwrap()
            .is_none()
    );
    let reserved = Workflow(&rig.store)
        .hold(question, 100, Request::ExplicitLaunch)
        .unwrap()
        .consent;
    let bound = rig
        .handle(
            Event::Bind {
                current: Box::new(current(&reserved)),
                reserved: Box::new(reserved),
                message: 40,
            },
            None,
        )
        .await
        .unwrap()
        .unwrap()
        .consent;
    let denied = rig
        .handle(Rig::decide(&bound, false), None)
        .await
        .unwrap()
        .unwrap()
        .consent;
    assert_eq!(denied.state, State::Denied);
    let mut changed = prepared.token.clone();
    changed.revision += 1;
    let revision = rig
        .handle(
            Event::Hold {
                question: Box::new(rig.question(&changed)),
                now: 102,
                request: Request::Automatic,
            },
            None,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        revision.effect,
        Effect::RevisionChanged,
        "engine_revision_change_no_automatic_card_contract"
    );
    assert!(rig.cards.sends.is_empty());
    assert!(rig.cards.replacements.last().unwrap().buttons.is_empty());
    let retried = rig
        .handle(
            Event::Retry {
                question: Box::new(rig.question(&changed)),
                actor: 1,
                owner: Some(1),
                local_node: "execution-node".into(),
                now: 103,
            },
            None,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(retried.effect, Effect::EnableButtons);
    assert_eq!(rig.cards.sends.len(), 1);
    assert_eq!(rig.calls(), 0);
    assert!(rig.custody.released.is_empty());
}
