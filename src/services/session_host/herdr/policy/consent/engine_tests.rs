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
    append_when_accepted: Option<(ConsentStore, Question)>,
}
#[async_trait::async_trait]
impl Owner for FakeOwner {
    async fn current(&mut self, consent: &Consent) -> Result<Current, String> {
        self.calls += 1;
        if self.fail || self.fail_after.is_some_and(|limit| self.calls > limit) {
            Err("current owner unavailable".into())
        } else {
            if consent.state == State::Accepted
                && let Some((store, question)) = self.append_when_accepted.take()
            {
                store.hold_unbound(question, 102)?;
            }
            Ok(current(consent))
        }
    }
}

#[derive(Default)]
struct FakeCards {
    sends: Vec<card::Card>,
    replacements: Vec<card::Card>,
    disabled: Vec<u64>,
    fail_send: Option<SendFailure>,
    fail_replace: Option<SendFailure>,
    fail_disable: Option<SendFailure>,
}
#[async_trait::async_trait]
impl Cards for FakeCards {
    async fn send(&mut self, card: &card::Card) -> Result<u64, SendFailure> {
        self.sends.push(card.clone());
        match self.fail_send.take() {
            Some(error) => Err(error),
            None => Ok(40),
        }
    }
    async fn replace(&mut self, _: u64, card: &card::Card) -> Result<(), SendFailure> {
        self.replacements.push(card.clone());
        match self.fail_replace.take() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
    async fn disable(&mut self, message: u64) -> Result<(), SendFailure> {
        self.disabled.push(message);
        match self.fail_disable.take() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

struct FakeCustody {
    store: ConsentStore,
    released: Vec<SettlementKey>,
    woken: Vec<SettlementKey>,
    events: Arc<Mutex<Vec<&'static str>>>,
}
#[async_trait::async_trait]
impl CustodySink for FakeCustody {
    async fn release(&mut self, key: &SettlementKey, _: &IntentRef) -> Result<(), String> {
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
    async fn wake(&mut self, key: &SettlementKey, _: &IntentRef) -> Result<(), String> {
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
    local_node: String,
    boot: Arc<BootState>,
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
        let boot = Arc::new(BootState::new());
        boot.try_claim().unwrap().complete();
        (
            Self {
                _root: root,
                store,
                source,
                owner: FakeOwner::default(),
                cards: FakeCards::default(),
                custody,
                checker_baseline,
                local_node: "execution-node".into(),
                boot,
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
        event: Event<'_>,
        prepared: Option<Prepared>,
    ) -> Result<Option<Transition>, String> {
        Engine {
            store: &self.store,
            boot: &self.boot,
            local_node: self.local_node.clone(),
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
    fn decide(consent: &Consent, yes: bool) -> Event<'_> {
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
    assert!(
        rig.cards
            .replacements
            .last()
            .is_some_and(|card| card.buttons.is_empty()
                && card.content.contains("카드 연결 실패")
                && card.content.contains("!herdr consent retry")),
        "engine_failed_bind_replaces_card_with_retry_guidance_contract"
    );
    assert!(rig.cards.disabled.is_empty());
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
    // An operator repairs the abandoned marker before boot recovery reads the last receipt.
    let marker = rig._root.path().join("consents").join(format!(
        "{}.pending",
        super::super::super::durable::key(&bound.key).unwrap()
    ));
    fs::remove_file(marker).unwrap();
    rig.boot = Arc::new(BootState::new());
    let boot = rig.boot.clone();
    let authority = boot.try_claim().unwrap();
    let recovered = rig
        .handle(Event::BootRecover(bound.key.clone(), &authority), None)
        .await
        .unwrap()
        .unwrap();
    authority.complete();
    assert_eq!(
        recovered.consent.state,
        State::ApplyFailed,
        "engine_publication_receipt_failure_has_owner_exit_contract"
    );
    let receipt = recovered.consent.apply.as_ref().unwrap();
    assert_eq!(
        receipt.before, receipt.after,
        "engine_interrupted_observes_current_config_contract"
    );
    assert!(receipt.backup.is_none());
    assert_eq!(
        receipt.detail.as_deref(),
        Some("apply interrupted; config state re-observed")
    );
    let retry = Event::Retry {
        question: Box::new(rig.question(&bound.preview)),
        actor: 1,
        owner: Some(1),
        local_node: "execution-node".into(),
        now: 103,
    };
    assert_eq!(
        rig.handle(retry, None).await.unwrap().unwrap().effect,
        Effect::EnableButtons,
        "engine_publication_receipt_failure_retry_contract"
    );
    assert!(rig.custody.released.is_empty());
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
    rig.local_node = "other-node".into();
    rig.owner.fail = true;
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
    rig.local_node = "execution-node".into();
    let owner_calls = rig.owner.calls;
    let recovered = rig.handle(Event::Recover(bound.key), None).await;
    assert!(
        recovered.is_ok(),
        "engine_recovery_independent_of_discord_owner_contract"
    );
    recovered.unwrap();
    assert_eq!(
        rig.owner.calls, owner_calls,
        "engine_recovery_does_not_query_discord_owner_contract"
    );
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

#[tokio::test]
async fn engine_send_failure_unreserves_and_next_explicit_launch_sends_once() {
    let (mut rig, prepared) = Rig::new(false, false).await;
    rig.cards.fail_send = Some(SendFailure::Definitive("definitive send rejection".into()));
    let question = rig.question(&prepared.token);
    let key = question.key.clone();
    let failed = rig
        .handle(
            Event::Hold {
                question: Box::new(question),
                now: 100,
                request: Request::ExplicitLaunch,
            },
            None,
        )
        .await;
    assert!(
        failed.is_err(),
        "engine_definitive_send_is_failure_contract"
    );
    let unsent = rig.store.load(&key).unwrap().unwrap();
    assert_eq!(
        unsent.card,
        super::super::CardState::Awaiting,
        "engine_definitive_send_unreserves_contract"
    );
    assert_eq!(unsent.message, 0);
    let bound = rig.bound(&prepared.token).await;
    assert_eq!(bound.card, super::super::CardState::Bound);
    assert_eq!(
        rig.cards.sends.len(),
        2,
        "engine_next_explicit_launch_sends_once_contract"
    );
    assert_eq!(rig.calls(), 0);
    assert!(rig.custody.released.is_empty());
}

#[tokio::test]
async fn engine_unknown_send_keeps_reservation_and_never_resends() {
    let (mut rig, prepared) = Rig::new(false, false).await;
    rig.cards.fail_send = Some(SendFailure::Unknown("response lost".into()));
    let question = rig.question(&prepared.token);
    let key = question.key.clone();
    let failed = rig
        .handle(
            Event::Hold {
                question: Box::new(question),
                now: 100,
                request: Request::ExplicitLaunch,
            },
            None,
        )
        .await;
    assert!(
        failed.is_err(),
        "engine_unknown_send_is_not_success_contract"
    );
    assert_eq!(
        rig.store.load(&key).unwrap().unwrap().card,
        super::super::CardState::Reserved,
        "engine_unknown_send_keeps_reservation_contract"
    );
    rig.handle(Event::Recover(key), None).await.unwrap();
    let held = rig.bound(&prepared.token).await;
    assert_eq!(held.message, 0);
    assert_eq!(
        rig.cards.sends.len(),
        1,
        "engine_unknown_send_never_resends_contract"
    );
    assert!(rig.custody.released.is_empty());
}

#[tokio::test]
async fn engine_bind_failure_preserves_original_error_and_reports_cleanup() {
    let (mut rig, prepared) = Rig::new(false, false).await;
    rig.owner.fail = true;
    rig.cards.fail_replace = Some(SendFailure::Definitive("replace cleanup failed".into()));
    rig.cards.fail_disable = Some(SendFailure::Unknown("disable cleanup failed".into()));
    let question = rig.question(&prepared.token);
    let failed = rig
        .handle(
            Event::Hold {
                question: Box::new(question),
                now: 100,
                request: Request::ExplicitLaunch,
            },
            None,
        )
        .await;
    assert!(
        failed.is_err(),
        "engine_bind_failure_is_not_success_contract"
    );
    let error = failed.unwrap_err();
    assert!(
        error.starts_with("current owner unavailable")
            && error.contains("replace cleanup failed")
            && error.contains("disable cleanup failed"),
        "engine_bind_failure_preserves_original_and_cleanup_contract"
    );
    assert!(
        error.contains("replace rejected") && error.contains("disable outcome unknown"),
        "engine_cleanup_outcome_classification_contract"
    );
    assert!(rig.cards.replacements.last().unwrap().buttons.is_empty());
    assert_eq!(rig.cards.disabled, vec![40]);
}

#[tokio::test]
async fn engine_concurrent_append_between_decide_and_apply_records_receipt() {
    for same_preview in [true, false] {
        let (mut rig, prepared) = Rig::new(false, false).await;
        let bound = rig.bound(&prepared.token).await;
        let mut question = rig.question(&prepared.token);
        question.intent.id = "concurrent".into();
        question.intent.fifo = 11;
        question.intent.channel = 4;
        question.channel = 4;
        if !same_preview {
            question.preview.revision += 1;
        }
        rig.owner.append_when_accepted = Some((
            ConsentStore::new(Some(rig._root.path().join("consents"))),
            question,
        ));
        let applied = rig
            .handle(Rig::decide(&bound, true), Some(prepared))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            applied.consent.state,
            State::Applied,
            "engine_concurrent_append_identity_not_full_consent_contract"
        );
        let current = rig.store.load(&bound.key).unwrap().unwrap();
        assert_eq!(
            current
                .intents
                .iter()
                .map(|i| i.id.as_str())
                .collect::<Vec<_>>(),
            ["original", "concurrent"],
            "engine_concurrent_append_preserves_fifo_contract"
        );
        assert_eq!(
            rig.calls(),
            2,
            "engine_concurrent_append_one_apply_contract"
        );
        assert!(current.apply.as_ref().unwrap().after.is_some());
        assert_eq!(
            rig.custody.released.len(),
            if same_preview { 2 } else { 1 },
            "engine_concurrent_append_release_matching_preview_only_contract"
        );
        assert!(
            rig.cards
                .replacements
                .last()
                .unwrap()
                .content
                .contains("설정을 껐습니다. 보류한 입력을 다시 확인해 세션을 엽니다.")
        );
        assert!(
            !rig.cards
                .replacements
                .last()
                .unwrap()
                .content
                .contains("이미 적용됨"),
            "engine_fresh_apply_uses_first_result_contract"
        );
    }
}

#[tokio::test]
async fn engine_explicit_bind_failure_also_replaces_the_unbound_card() {
    let (mut rig, prepared) = Rig::new(false, false).await;
    let reserved = Workflow(&rig.store)
        .hold(rig.question(&prepared.token), 100, Request::ExplicitLaunch)
        .unwrap()
        .consent;
    let mut changed = current(&reserved);
    changed.owner = None;
    let failed = rig
        .handle(
            Event::Bind {
                reserved: Box::new(reserved),
                current: Box::new(changed),
                message: 40,
            },
            None,
        )
        .await;
    assert!(
        failed.is_err(),
        "engine_explicit_bind_failure_is_not_success_contract"
    );
    assert!(
        rig.cards
            .replacements
            .last()
            .is_some_and(|card| card.buttons.is_empty() && card.content.contains("카드 연결 실패")),
        "engine_explicit_bind_failure_replaces_card_contract"
    );
    assert!(rig.custody.released.is_empty());
}

#[tokio::test]
async fn engine_duplicate_bind_is_idempotent_and_keeps_live_buttons() {
    let (mut rig, prepared) = Rig::new(false, false).await;
    let reserved = Workflow(&rig.store)
        .hold(rig.question(&prepared.token), 100, Request::ExplicitLaunch)
        .unwrap()
        .consent;
    let first = rig
        .handle(
            Event::Bind {
                reserved: Box::new(reserved.clone()),
                current: Box::new(current(&reserved)),
                message: 40,
            },
            None,
        )
        .await
        .unwrap()
        .unwrap();
    let replacements = rig.cards.replacements.len();
    let duplicate = rig
        .handle(
            Event::Bind {
                reserved: Box::new(reserved.clone()),
                current: Box::new(current(&reserved)),
                message: 40,
            },
            None,
        )
        .await;
    assert!(
        duplicate.is_ok(),
        "engine_duplicate_bind_keeps_live_card_contract: {duplicate:?}"
    );
    let duplicate = duplicate.unwrap().unwrap();
    assert_eq!(
        duplicate.effect,
        Effect::KeepHeld,
        "engine_duplicate_bind_keeps_live_card_contract"
    );
    assert_eq!(duplicate.consent, first.consent);
    assert_eq!(
        rig.cards.replacements.len(),
        replacements,
        "engine_duplicate_bind_does_not_replace_buttons_contract"
    );
    assert!(
        rig.cards
            .replacements
            .iter()
            .all(|card| !card.buttons.is_empty())
    );
    assert!(rig.cards.disabled.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn engine_non_boot_recover_never_interrupts_an_in_flight_apply() {
    use std::time::Duration;
    let (mut rig, prepared) = Rig::new(false, false).await;
    let checker = &rig.source.0.checker;
    let body = fs::read_to_string(checker).unwrap();
    let barrier = "if [ -f \"$HOME/engine-ready\" ]; then case \"$HERDR_CONFIG_PATH\" in \"$HOME/config.toml\") ;; *) touch \"$HOME/barrier-entered\"; while [ ! -f \"$HOME/barrier-release\" ]; do sleep 0.01; done;; esac; fi\n";
    fs::write(
        checker,
        body.replace(
            "printf 'config: ok\\n'",
            &format!("{barrier}printf 'config: ok\\n'"),
        ),
    )
    .unwrap();
    // Refresh only the prepared executable fingerprint after changing the fake checker.
    let mut snapshot = rig.source.0.clone();
    snapshot.image = file::image(checker).unwrap();
    rig.source = Arc::new(Source(snapshot));
    fs::remove_file(rig.source.0.home.join("engine-ready")).unwrap();
    let prepared = preview::prepare(rig.source.clone(), prepared.token.revision)
        .await
        .unwrap();
    fs::write(rig.source.0.home.join("engine-ready"), "ready").unwrap();
    let bound = rig.bound(&prepared.token).await;
    let path = rig.source.0.home.clone();
    let directory = path.join("consents");
    let source = rig.source.clone();
    let boot = rig.boot.clone();
    let key = bound.key.clone();
    let handle = tokio::spawn(async move {
        let applied = rig.handle(Rig::decide(&bound, true), Some(prepared)).await;
        (rig, applied)
    });
    let entered = tokio::time::timeout(Duration::from_secs(1), async {
        while !path.join("barrier-entered").exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(
        entered.is_ok(),
        "engine_nonboot_recovery_checker_barrier_is_reached_contract"
    );
    let store = ConsentStore::new(Some(directory.clone()));
    let mut owner = FakeOwner::default();
    let mut cards = FakeCards::default();
    let mut custody = FakeCustody {
        store: ConsentStore::new(Some(directory)),
        released: Vec::new(),
        woken: Vec::new(),
        events: Arc::new(Mutex::new(Vec::new())),
    };
    let recovered = Engine {
        store: &store,
        boot: &boot,
        local_node: key.node.clone(),
        source,
        owner: &mut owner,
        cards: &mut cards,
        custody: &mut custody,
    }
    .handle(Event::Recover(key.clone()), None)
    .await
    .unwrap()
    .unwrap();
    fs::write(path.join("barrier-release"), "release").unwrap();
    let (rig, applied) = handle.await.unwrap();
    assert_eq!(
        recovered.consent.state,
        State::Accepted,
        "engine_nonboot_recover_never_interrupts_accepted_contract"
    );
    assert_eq!(recovered.effect, Effect::KeepHeld);
    let applied = applied.unwrap().unwrap();
    assert_eq!(
        applied.consent.state,
        State::Applied,
        "engine_nonboot_recovery_keeps_apply_receipt_contract"
    );
    assert_eq!(
        rig.custody.released.len(),
        1,
        "engine_nonboot_recovery_releases_once_contract"
    );
}

#[tokio::test]
async fn engine_boot_authority_is_single_use_and_failed_boot_never_admits_apply() {
    let boot = BootState::new();
    assert!(boot.require_ready().is_err());
    let authority = boot.try_claim().unwrap();
    assert!(
        boot.try_claim().is_none(),
        "engine_boot_authority_claim_once_contract"
    );
    assert!(
        boot.require_ready().is_err(),
        "engine_boot_authority_blocks_apply_until_complete_contract"
    );
    authority.complete();
    assert!(boot.is_ready());
    assert!(boot.try_claim().is_none());
    let failed = BootState::new();
    drop(failed.try_claim().unwrap());
    assert!(
        failed.require_ready().is_err(),
        "engine_failed_boot_keeps_apply_refused_contract"
    );
    assert!(failed.try_claim().is_none());
}

#[tokio::test]
async fn engine_apply_waits_for_completed_boot_authority() {
    let (mut rig, prepared) = Rig::new(false, false).await;
    let bound = rig.bound(&prepared.token).await;
    rig.boot = Arc::new(BootState::new());
    let boot = rig.boot.clone();
    let authority = boot.try_claim().unwrap();
    let refused = rig
        .handle(Rig::decide(&bound, true), Some(prepared.clone()))
        .await;
    assert!(
        matches!(refused, Err(ref error) if error.contains("boot recovery is not complete")),
        "engine_boot_recovery_precedes_apply_contract"
    );
    assert_eq!(
        rig.store.load(&bound.key).unwrap().unwrap().state,
        State::Pending
    );
    assert_eq!(
        rig.calls(),
        0,
        "engine_boot_recovery_before_checker_contract"
    );
    assert!(rig.custody.released.is_empty());
    authority.complete();
    let applied = rig
        .handle(Rig::decide(&bound, true), Some(prepared))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(applied.consent.state, State::Applied);
}

#[tokio::test]
async fn engine_owner_retry_of_verified_false_releases_without_a_card() {
    let (mut rig, _) = Rig::new(false, false).await;
    let bytes = b"[session]\nresume_agents_on_restore = false\n";
    fs::write(&rig.source.0.config, bytes).unwrap();
    let first = preview::prepare(rig.source.clone(), 1).await.unwrap();
    let automatic = rig
        .handle(
            Event::Hold {
                question: Box::new(rig.question(&first.token)),
                now: 100,
                request: Request::Automatic,
            },
            None,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(automatic.effect, Effect::KeepHeld);
    let prepared = preview::prepare(rig.source.clone(), 2).await.unwrap();
    assert!(!prepared.plan.requires_write());
    let rejected = rig
        .handle(
            Event::Retry {
                question: Box::new(rig.question(&prepared.token)),
                actor: 2,
                owner: Some(1),
                local_node: "execution-node".into(),
                now: 101,
            },
            Some(prepared.clone()),
        )
        .await;
    assert!(
        matches!(rejected, Err(ref error) if error.contains("current owner")),
        "engine_verified_false_retry_still_requires_owner_contract"
    );
    assert!(rig.cards.sends.is_empty());
    let retried = rig
        .handle(
            Event::Retry {
                question: Box::new(rig.question(&prepared.token)),
                actor: 1,
                owner: Some(1),
                local_node: "execution-node".into(),
                now: 102,
            },
            Some(prepared),
        )
        .await;
    assert!(
        retried.is_ok(),
        "engine_verified_false_owner_retry_completes_contract: {retried:?}"
    );
    let applied = retried.unwrap().unwrap().consent;
    assert!(
        rig.cards.sends.is_empty(),
        "engine_verified_false_retry_card_zero_contract"
    );
    assert_eq!(
        applied.state,
        State::Applied,
        "engine_verified_false_retry_records_receipt_contract"
    );
    assert_eq!(
        fs::read(&rig.source.0.config).unwrap(),
        bytes,
        "engine_verified_false_retry_does_not_rewrite_config_contract"
    );
    assert!(
        applied.apply.as_ref().unwrap().backup.is_none(),
        "engine_verified_false_retry_backup_zero_contract"
    );
    assert_eq!(applied.message, 0);
    assert_eq!(
        rig.custody.released.len(),
        1,
        "engine_verified_false_retry_release_once_contract"
    );
    rig.handle(Event::Recover(applied.key), None).await.unwrap();
    assert_eq!(rig.custody.released.len(), 1);
}
