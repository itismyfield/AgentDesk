//! Process-wide effect observation for isolated input-owner acceptance fixtures.
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::thread::{ThreadId, current};

use serde_json::Value;

#[path = "transport_tests.rs"]
mod transport;
#[cfg(unix)]
pub(crate) use transport::herdr;
pub(crate) use transport::{begin_load_buffer, tmux};
#[path = "bounded_tests.rs"]
pub(crate) mod bounded;
#[cfg(unix)]
pub(crate) use bounded::observe_bounded;
pub(crate) use bounded::{capture_transport_context, producer_effect};
#[cfg(unix)]
mod repair_tests;
#[path = "scenarios_tests.rs"]
pub(crate) mod scenarios;
mod tests;
#[cfg(unix)]
pub(crate) use tests::fixture;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Target {
    pub provider: String,
    pub namespace: String,
    pub root: String,
    pub host: String,
    pub physical: String,
    pub execution: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Attempt {
    pub target: Target,
    pub sources: Vec<(u64, u64)>,
    pub primary: (u64, u64),
    pub nonce: String,
    pub native_source: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Scope {
    Owner(Box<Attempt>),
    Preserved4h(String),
}

impl Scope {
    pub(crate) fn owner(attempt: Attempt) -> Self {
        Self::Owner(Box::new(attempt))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Op {
    Literal,
    Paste,
    Argv,
    Text,
    Line,
    Enter,
    Create,
    Capture,
    Load,
    Control,
    Unclassified,
}

pub(super) fn digest(bytes: &[u8]) -> String {
    use sha2::Digest;
    format!("{:x}", sha2::Sha256::digest(bytes))
}

impl Op {
    fn content(self) -> bool {
        matches!(
            self,
            Self::Literal | Self::Paste | Self::Argv | Self::Text | Self::Line
        )
    }
    fn guarded(self) -> bool {
        self.content() || matches!(self, Self::Enter | Self::Create | Self::Unclassified)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Event {
    pub op: Op,
    pub scope: Option<Scope>,
    pub accepted: bool,
    pub written: bool,
    pub bytes: usize,
    pub digest: String,
}

#[derive(Clone, Default)]
pub(crate) struct Counts {
    pub entry_reached: usize,
    pub admissions: HashMap<Attempt, usize>,
    pub owner_content: HashMap<Attempt, usize>,
    pub preserved_4h_content: HashMap<String, usize>,
    pub attempted_legacy: usize,
    pub max_writers: usize,
    pub events: Vec<Event>,
}

struct State {
    target: Target,
    counts: Counts,
    proofs: Vec<(Attempt, bool)>,
    preserved: HashMap<String, (Target, Vec<u8>)>,
    argv: HashMap<String, Vec<u8>>,
    scopes: HashMap<ThreadId, Vec<Scope>>,
    writers: usize,
}

type Shared = Arc<Mutex<State>>;
fn registry() -> &'static Mutex<Vec<Shared>> {
    static REGISTRY: OnceLock<Mutex<Vec<Shared>>> = OnceLock::new();
    REGISTRY.get_or_init(Default::default)
}

pub(crate) struct StrictOwnerEffectGuard(Shared);

impl StrictOwnerEffectGuard {
    pub(crate) fn new(target: Target) -> Self {
        assert!(
            [
                &target.provider,
                &target.namespace,
                &target.root,
                &target.host,
                &target.physical,
                &target.execution
            ]
            .iter()
            .all(|s| !s.is_empty())
        );
        let mut entries = registry().lock().unwrap_or_else(PoisonError::into_inner);
        assert!(
            !entries.iter().any(|entry| {
                let state = entry.lock().unwrap_or_else(PoisonError::into_inner);
                state.target.host == target.host && state.target.physical == target.physical
            }),
            "duplicate physical target registration"
        );
        let state = Arc::new(Mutex::new(State {
            target,
            counts: Counts::default(),
            proofs: vec![],
            preserved: HashMap::new(),
            argv: HashMap::new(),
            scopes: HashMap::new(),
            writers: 0,
        }));
        entries.push(state.clone());
        Self(state)
    }

    pub(crate) fn rebind(&self, target: Target) -> Result<(), &'static str> {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if state.writers != 0
            || target.host != state.target.host
            || target.physical != state.target.physical
            || [
                &target.provider,
                &target.namespace,
                &target.root,
                &target.execution,
            ]
            .iter()
            .any(|s| s.is_empty())
        {
            return Err("rebind requires the same protected physical target and no live writer");
        }
        state.target = target;
        Ok(())
    }

    pub(crate) fn entry_reached(&self) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .counts
            .entry_reached += 1;
    }

    pub(crate) fn admission(&self, attempt: &Attempt) {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        assert_eq!(state.target, attempt.target);
        assert!(!attempt.sources.is_empty() && !attempt.nonce.is_empty());
        *state.counts.admissions.entry(attempt.clone()).or_default() += 1;
    }

    // This consumes persisted fixture evidence, never a guessed future owner capability.
    pub(crate) fn observe_durable(&self, attempt: &Attempt, path: &Path) -> Result<(), String> {
        let raw = std::fs::read(path).map_err(|error| error.to_string())?;
        let value: Value = serde_json::from_slice(&raw).map_err(|error| error.to_string())?;
        let head = &value["head_injection"];
        let actual = &head["attempt"];
        let target = &actual["target"];
        let sources: Vec<(u64, u64)> = head["identity"]["sources"]
            .as_array()
            .ok_or("missing sources")?
            .iter()
            .map(|v| {
                Ok((
                    v["message_id"].as_u64().ok_or("source")?,
                    v["queued_generation"].as_u64().ok_or("generation")?,
                ))
            })
            .collect::<Result<_, String>>()?;
        let expected_source: Value =
            serde_json::from_str(&attempt.native_source).map_err(|error| error.to_string())?;
        let window = &head["payload_window"];
        let started = window["kind"] == "started"
            || (window.is_null() && head["confirmation_deadline_ms"].as_u64().is_some());
        let bootstrap = window["kind"] == "bootstrap";
        if value["version"] != 2
            || value["namespace"] != attempt.target.namespace
            || actual["nonce"] != attempt.nonce
            || actual["provider"] != attempt.target.provider
            || actual["source"] != expected_source
            || target["host"] != attempt.target.host
            || target["owner"] != attempt.target.physical
            || target["root"] != attempt.target.root
            || target["execution_nonce"] != attempt.target.execution
            || sources != attempt.sources
            || head["identity"]["primary"] != attempt.primary.0
            || head["identity"]["queued_generation"] != attempt.primary.1
            || head["payload"]["frame"].as_str().is_none_or(str::is_empty)
            || head["phase"] != "may_have_sent"
            || (!started && !bootstrap)
        {
            return Err("missing exact durable payload-start evidence".into());
        }
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if state.target != attempt.target || !state.counts.admissions.contains_key(attempt) {
            return Err("unadmitted or foreign proof".into());
        }
        state.proofs.push((attempt.clone(), started));
        Ok(())
    }

    pub(crate) fn expect_argv(&self, command: &str, prompt: &[u8]) {
        assert!(!command.is_empty() && !prompt.is_empty());
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .argv
            .insert(command.into(), prompt.into());
    }

    pub(crate) fn preserve_4h(&self, origin: &str, payload: &[u8]) {
        assert!(!origin.is_empty() && !payload.is_empty());
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let target = state.target.clone();
        state
            .preserved
            .insert(origin.into(), (target, payload.into()));
    }

    pub(crate) fn scope(&self, scope: Scope) -> ScopeGuard {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .scopes
            .entry(current().id())
            .or_default()
            .push(scope);
        ScopeGuard(self.0.clone(), std::marker::PhantomData)
    }

    pub(crate) fn counts(&self) -> Counts {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .counts
            .clone()
    }

    pub(crate) fn assert_owner_content(&self, attempt: &Attempt, expected: usize) {
        let counts = self.counts();
        assert!(counts.entry_reached > 0);
        assert!(counts.admissions.get(attempt).copied().unwrap_or(0) > 0);
        assert!(expected > 0);
        assert_eq!(
            counts.owner_content.get(attempt).copied().unwrap_or(0),
            expected
        );
        assert_eq!(
            counts.attempted_legacy, 0,
            "fallback attempted a protected leaf"
        );
    }
    pub(crate) fn assert_owner_payload(&self, attempt: &Attempt, frame: &[u8], expected: usize) {
        assert!(!frame.is_empty());
        self.assert_owner_content(attempt, expected);
        let expected_digest = digest(frame);
        let counts = self.counts();
        let matching = counts
            .events
            .iter()
            .filter(|event| {
                event.written
                    && event.op.content()
                    && matches!(&event.scope, Some(Scope::Owner(actual)) if actual.as_ref() == attempt)
                    && event.digest == expected_digest
            })
            .count();
        assert_eq!(
            matching, expected,
            "auxiliary text cannot substitute for the prepared payload"
        );
    }
}

impl Drop for StrictOwnerEffectGuard {
    fn drop(&mut self) {
        let mut entries = registry().lock().unwrap_or_else(PoisonError::into_inner);
        entries.retain(|entry| !Arc::ptr_eq(entry, &self.0));
        if entries.is_empty() {
            transport::clear_buffers();
        }
    }
}

pub(crate) struct ScopeGuard(Shared, std::marker::PhantomData<std::rc::Rc<()>>);
impl Drop for ScopeGuard {
    fn drop(&mut self) {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(scopes) = state.scopes.get_mut(&current().id()) {
            scopes.pop();
        }
    }
}

// Keep the interval alive until the actual command/socket write returns.
#[derive(Default)]
pub(crate) struct Flight(Option<(Shared, usize)>);
impl Flight {
    pub(crate) fn written(&self) {
        let Some((state, index)) = &self.0 else {
            return;
        };
        let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
        let event = &mut state.counts.events[*index];
        if event.written {
            return;
        }
        event.written = true;
        if !event.op.content() {
            return;
        }
        match event.scope.clone() {
            Some(Scope::Owner(attempt)) => {
                *state.counts.owner_content.entry(*attempt).or_default() += 1
            }
            Some(Scope::Preserved4h(origin)) => {
                *state.counts.preserved_4h_content.entry(origin).or_default() += 1
            }
            None => unreachable!("written content requires scope"),
        }
    }
}
impl Drop for Flight {
    fn drop(&mut self) {
        if let Some((state, _)) = &self.0 {
            state.lock().unwrap_or_else(PoisonError::into_inner).writers -= 1;
        }
    }
}

pub(crate) fn before(host: &str, physical: &str, op: Op, payload: &[u8]) -> Result<Flight, String> {
    let entry = registry()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .find(|entry| {
            let state = entry.lock().unwrap_or_else(PoisonError::into_inner);
            state.target.host == host && state.target.physical == physical
        })
        .cloned();
    let Some(entry) = entry else {
        return Ok(Flight::default());
    };
    let mut state = entry.lock().unwrap_or_else(PoisonError::into_inner);
    let scope = state
        .scopes
        .get(&current().id())
        .and_then(|v| v.last())
        .cloned();
    let authorized = match &scope {
        Some(Scope::Owner(attempt)) => state.proofs.iter().any(|(proof, started)| {
            attempt.target == state.target
                && proof == attempt.as_ref()
                && (*started || op == Op::Create)
        }),
        Some(Scope::Preserved4h(origin)) => {
            state
                .preserved
                .get(origin)
                .is_some_and(|(target, expected)| {
                    target == &state.target && (!op.content() || expected == payload)
                })
        }
        None => false,
    };
    let accepted = !op.guarded() || (op != Op::Unclassified && authorized && state.writers == 0);
    state.counts.events.push(Event {
        op,
        scope: scope.clone(),
        accepted,
        written: false,
        bytes: payload.len(),
        digest: digest(payload),
    });
    if !accepted {
        state.counts.attempted_legacy += 1;
        return Err("B0 refused unscoped, foreign, unproved or overlapping effect".into());
    }
    if op.guarded() {
        state.writers += 1;
        state.counts.max_writers = state.counts.max_writers.max(state.writers);
        let index = state.counts.events.len() - 1;
        drop(state);
        Ok(Flight(Some((entry, index))))
    } else {
        Ok(Flight::default())
    }
}

#[test]
fn s5_b0_durable_proof_rejects_missing_and_foreign_fields_without_effects() {
    let (root, attempt, guard) = tests::fixture("fixture", &uuid::Uuid::new_v4().to_string());
    let path = root.path().join("proof.json");
    let original = std::fs::read(&path).unwrap();
    for pointer in [
        "/namespace",
        "/head_injection/attempt/nonce",
        "/head_injection/attempt/source",
        "/head_injection/attempt/target/execution_nonce",
        "/head_injection/identity/primary",
        "/head_injection/payload/frame",
        "/head_injection/payload_window",
    ] {
        let mut changed: Value = serde_json::from_slice(&original).unwrap();
        *changed.pointer_mut(pointer).unwrap() = Value::Null;
        let bytes = changed.to_string();
        std::fs::write(&path, &bytes).unwrap();
        assert!(
            guard.observe_durable(&attempt, &path).is_err(),
            "missing {pointer}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), bytes);
    }
    let missing = root.path().join("missing.json");
    assert!(guard.observe_durable(&attempt, &missing).is_err());
    assert!(guard.counts().owner_content.is_empty());
}

#[test]
fn s5_b0_bootstrap_proof_allows_create_but_never_content_before_payload_start() {
    let (root, original, guard) = tests::fixture("fixture", &uuid::Uuid::new_v4().to_string());
    let mut attempt = original;
    attempt.nonce = "bootstrap-attempt".into();
    guard.admission(&attempt);
    let path = root.path().join("proof.json");
    let mut value: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    value["head_injection"]["attempt"]["nonce"] = attempt.nonce.clone().into();
    value["head_injection"]["payload_window"] = serde_json::json!({"kind":"bootstrap"});
    std::fs::write(&path, value.to_string()).unwrap();
    guard.observe_durable(&attempt, &path).unwrap();
    let _scope = guard.scope(Scope::owner(attempt.clone()));
    before("fixture", &attempt.target.physical, Op::Create, b"")
        .unwrap()
        .written();
    assert!(before("fixture", &attempt.target.physical, Op::Text, b"too early").is_err());
    assert!(guard.counts().owner_content.is_empty());
    value["head_injection"]["payload_window"] = serde_json::json!({"kind":"started","at_ms":1000});
    std::fs::write(&path, value.to_string()).unwrap();
    guard.observe_durable(&attempt, &path).unwrap();
    before("fixture", &attempt.target.physical, Op::Text, b"started")
        .unwrap()
        .written();
    assert_eq!(guard.counts().owner_content.get(&attempt), Some(&1));
    assert_eq!(guard.counts().attempted_legacy, 1);
}

#[test]
fn s5_b0_rebind_keeps_physical_guard_and_rejects_the_previous_execution_scope() {
    let (root, a, guard) = tests::fixture("fixture", &uuid::Uuid::new_v4().to_string());
    guard.preserve_4h("old-origin", b"preserved");
    let scope_a = guard.scope(Scope::owner(a.clone()));
    let flight = before("fixture", &a.target.physical, Op::Text, b"owner").unwrap();
    flight.written();
    let mut b = a.clone();
    b.nonce = "attempt-B".into();
    b.sources = vec![(200, 5), (201, 6)];
    b.primary = (200, 5);
    b.target.execution = "execution-B".into();
    assert!(guard.rebind(b.target.clone()).is_err());
    drop(flight);
    guard.rebind(b.target.clone()).unwrap();
    assert!(before("fixture", &a.target.physical, Op::Text, b"late A").is_err());
    drop(scope_a);
    assert!(
        before(
            "fixture",
            &b.target.physical,
            Op::Text,
            b"unscoped after restart"
        )
        .is_err()
    );
    {
        let _old_4h = guard.scope(Scope::Preserved4h("old-origin".into()));
        assert!(before("fixture", &b.target.physical, Op::Text, b"preserved").is_err());
    }
    guard.entry_reached();
    guard.admission(&b);
    let path = root.path().join("proof.json");
    let mut value: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    value["head_injection"]["identity"] = serde_json::json!({"primary":200,"queued_generation":5,
        "sources":[{"message_id":200,"queued_generation":5},{"message_id":201,"queued_generation":6}]});
    value["head_injection"]["attempt"]["nonce"] = b.nonce.clone().into();
    value["head_injection"]["attempt"]["target"]["execution_nonce"] =
        b.target.execution.clone().into();
    std::fs::write(&path, value.to_string()).unwrap();
    guard.observe_durable(&b, &path).unwrap();
    let _scope_b = guard.scope(Scope::owner(b.clone()));
    before("fixture", &b.target.physical, Op::Text, b"owner")
        .unwrap()
        .written();
    let counts = guard.counts();
    assert_eq!(counts.owner_content.get(&a), Some(&1));
    assert_eq!(counts.owner_content.get(&b), Some(&1));
    assert_eq!(counts.attempted_legacy, 3);
}
