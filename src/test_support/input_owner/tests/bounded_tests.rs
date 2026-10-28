use super::{Op, Scope, ScopeGuard, Shared, before, registry};
use std::collections::HashMap;
use std::marker::PhantomData;
#[cfg(unix)]
use std::process::{Command, Output};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::thread::{ThreadId, current};

#[derive(Clone)]
struct Declared {
    physical: String,
    op: Op,
    payload: Vec<u8>,
    applied: String,
    buffer: Option<String>,
}

fn declarations() -> &'static Mutex<HashMap<ThreadId, Vec<Declared>>> {
    static DECLARATIONS: OnceLock<Mutex<HashMap<ThreadId, Vec<Declared>>>> = OnceLock::new();
    DECLARATIONS.get_or_init(Default::default)
}

fn declaration() -> Option<Declared> {
    declarations()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&current().id())
        .and_then(|stack| stack.last())
        .cloned()
}

pub(crate) struct ProducerEffect(PhantomData<std::rc::Rc<()>>);

fn push(effect: Declared) -> ProducerEffect {
    declarations()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .entry(current().id())
        .or_default()
        .push(effect);
    ProducerEffect(PhantomData)
}

pub(crate) fn producer_effect(
    target: &str,
    op: Op,
    payload: &[u8],
    applied: &str,
    buffer: Option<&str>,
) -> ProducerEffect {
    push(Declared {
        physical: target.trim_start_matches('=').trim_end_matches(':').into(),
        op,
        payload: payload.into(),
        applied: applied.into(),
        buffer: buffer.map(String::from),
    })
}

impl Drop for ProducerEffect {
    fn drop(&mut self) {
        let mut declarations = declarations()
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(stack) = declarations.get_mut(&current().id()) {
            stack.pop();
            if stack.is_empty() {
                declarations.remove(&current().id());
            }
        }
    }
}

pub(crate) struct TransportContext {
    scopes: Vec<(Shared, Scope)>,
    effect: Option<Declared>,
}

pub(crate) fn capture_transport_context() -> TransportContext {
    let scopes = registry()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .filter_map(|entry| {
            let state = entry.lock().unwrap_or_else(PoisonError::into_inner);
            let scope = state.scopes.get(&current().id())?.last()?.clone();
            Some((entry.clone(), scope))
        })
        .collect();
    TransportContext {
        scopes,
        effect: declaration(),
    }
}

pub(crate) struct RestoredContext {
    _scopes: Vec<ScopeGuard>,
    _effect: Option<ProducerEffect>,
}

impl TransportContext {
    pub(crate) fn enter(self) -> RestoredContext {
        RestoredContext {
            _scopes: self
                .scopes
                .into_iter()
                .map(|(shared, scope)| {
                    shared
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .scopes
                        .entry(current().id())
                        .or_default()
                        .push(scope);
                    ScopeGuard(shared, PhantomData)
                })
                .collect(),
            _effect: self.effect.map(|effect| {
                producer_effect(
                    &effect.physical,
                    effect.op,
                    &effect.payload,
                    &effect.applied,
                    effect.buffer.as_deref(),
                )
            }),
        }
    }
}

#[cfg(unix)]
#[derive(Default)]
pub(crate) struct BoundedObservation {
    flight: super::Flight,
    applied: Option<String>,
    load: Option<super::transport::BufferLoad>,
}

#[cfg(unix)]
pub(crate) fn observe_bounded(command: &Command) -> Result<BoundedObservation, String> {
    let args: Vec<_> = command
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let verb = super::transport::verb(&args);
    let pinned = verb == "if-shell";
    let effect = declaration().filter(|effect| {
        pinned
            || match effect.op {
                Op::Paste => verb == "paste-buffer",
                Op::Enter => {
                    verb == "send-keys"
                        && !args.iter().any(|arg| arg == "-l")
                        && args.iter().any(|arg| arg == "Enter")
                }
                _ => false,
            }
    });
    let (flight, applied) = if let Some(effect) = effect {
        let payload = if effect.op == Op::Paste {
            effect
                .buffer
                .as_deref()
                .and_then(super::transport::buffer_text)
                .filter(|actual| actual.as_bytes() == effect.payload)
                .unwrap_or_default()
                .into_bytes()
        } else {
            effect.payload
        };
        (
            before("tmux", &effect.physical, effect.op, &payload)?,
            pinned.then_some(effect.applied),
        )
    } else {
        (super::tmux(command)?, None)
    };
    let load = if verb == "load-buffer" {
        let name = args
            .windows(2)
            .find(|pair| pair[0] == "-b")
            .map(|pair| pair[1].as_str());
        name.zip(args.last()).map(|(name, path)| {
            let bytes = std::fs::read_to_string(&path[..]);
            let mut load =
                super::transport::begin_load_buffer(name, bytes.as_deref().unwrap_or(""));
            if bytes.is_ok() {
                load.input_complete();
            }
            load
        })
    } else {
        None
    };
    Ok(BoundedObservation {
        flight,
        applied,
        load,
    })
}

#[cfg(unix)]
impl BoundedObservation {
    pub(crate) fn completed(&mut self, output: &Output) {
        if !output.status.success() {
            return;
        }
        if let Some(load) = &mut self.load {
            load.succeeded();
        }
        if self
            .applied
            .as_deref()
            .is_none_or(|marker| String::from_utf8_lossy(&output.stdout).trim() == marker)
        {
            self.flight.written();
        }
    }
}

#[test]
fn s5_b0_worker_context_requires_explicit_capture_and_restore() {
    let (_root, attempt, guard) =
        super::tests::fixture("fixture", &uuid::Uuid::new_v4().to_string());
    let context = {
        let _scope = guard.scope(Scope::owner(attempt.clone()));
        capture_transport_context()
    };
    std::thread::scope(|threads| {
        threads
            .spawn(|| {
                let _scope = context.enter();
                before("fixture", &attempt.target.physical, Op::Text, b"frame")
                    .unwrap()
                    .written();
            })
            .join()
            .unwrap();
    });
    guard.assert_owner_payload(&attempt, b"frame", 1);
}
