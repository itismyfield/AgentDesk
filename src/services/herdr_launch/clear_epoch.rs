//! Channel clear and physical Herdr effects share one permission boundary.
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, Weak};

use crate::db::dispatched_sessions::hosted_execution::HostedOwner;

type Key = (String, String, u64);
static CELLS: LazyLock<Mutex<HashMap<Key, Weak<Cell>>>> = LazyLock::new(Default::default);
thread_local! {
    static CURRENT: RefCell<Option<Arc<Scope>>> = const { RefCell::new(None) };
}

#[derive(Default)]
struct State {
    epoch: u64,
    clearing: usize,
    startups: usize,
    effects: usize,
    overlap: bool,
}

#[derive(Default)]
struct Cell {
    state: Mutex<State>,
    changed: tokio::sync::Notify,
}

fn cell(key: &Key) -> Arc<Cell> {
    let mut cells = CELLS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(cell) = cells.get(key).and_then(Weak::upgrade) {
        return cell;
    }
    cells.retain(|_, cell| cell.strong_count() != 0);
    let cell = Arc::new(Cell::default());
    cells.insert(key.clone(), Arc::downgrade(&cell));
    cell
}

/// The restart postlude keeps this cell alive across a predecessor's exact termination.
#[cfg(unix)]
pub(crate) struct Epoch(Arc<Cell>, Option<u64>);

#[cfg(unix)]
pub(crate) fn snapshot(provider: &str, token_hash: &str, channel: u64) -> Epoch {
    let cell = cell(&(provider.into(), token_hash.into(), channel));
    let epoch = {
        let state = cell.state.lock().unwrap_or_else(|e| e.into_inner());
        (state.clearing == 0).then_some(state.epoch)
    };
    Epoch(cell, epoch)
}

#[cfg(unix)]
impl Epoch {
    pub(crate) fn unchanged(&self) -> bool {
        let state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        state.clearing == 0 && self.1 == Some(state.epoch)
    }
}

pub(crate) struct ClearGuard(Arc<Cell>);

pub(crate) fn begin_clear(provider: &str, token_hash: &str, channel: u64) -> ClearGuard {
    let cell = cell(&(provider.into(), token_hash.into(), channel));
    {
        let mut state = cell.state.lock().unwrap_or_else(|e| e.into_inner());
        state.epoch = state.epoch.checked_add(1).expect("clear epoch exhausted");
        state.clearing += 1;
        state.overlap |= state.startups != 0 || state.effects != 0;
    }
    ClearGuard(cell)
}

impl ClearGuard {
    #[cfg(any(unix, test))]
    pub(crate) fn overlapped_startup(&self) -> bool {
        self.0
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .overlap
    }

    pub(crate) async fn wait_idle(&self) {
        loop {
            let notified = self.0.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
                if state.startups == 0 && state.effects == 0 {
                    return;
                }
            }
            notified.await;
        }
    }
}

impl Drop for ClearGuard {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        state.clearing -= 1;
        if state.clearing == 0 {
            state.overlap = false;
        }
    }
}

pub(crate) struct Scope {
    key: Key,
    cell: Arc<Cell>,
    epoch: u64,
    finished: AtomicBool,
}

impl Scope {
    fn register(key: Key) -> Result<Arc<Self>, String> {
        let cell = cell(&key);
        let epoch = {
            let mut state = cell.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.clearing != 0 {
                return Err("herdr clear is in progress".into());
            }
            state.startups += 1;
            state.epoch
        };
        Ok(Arc::new(Self {
            key,
            cell,
            epoch,
            finished: AtomicBool::new(false),
        }))
    }

    pub(crate) fn check_epoch(self: &Arc<Self>) -> Result<Checked, String> {
        let state = self.cell.state.lock().unwrap_or_else(|e| e.into_inner());
        self.allowed(&state)?;
        drop(state);
        #[cfg(test)]
        tests::point(&self.key, tests::Point::Checked);
        Ok(Checked(Arc::clone(self)))
    }

    fn allowed(&self, state: &State) -> Result<(), String> {
        if state.epoch != self.epoch || state.clearing != 0 {
            return Err("herdr clear changed the launch epoch".into());
        }
        Ok(())
    }

    pub(crate) fn finish_start(&self) {
        if !self.finished.swap(true, Ordering::AcqRel) {
            self.cell
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .startups -= 1;
            self.cell.changed.notify_waiters();
        }
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        self.finish_start();
    }
}

pub(crate) struct Checked(Arc<Scope>);
pub(crate) struct EffectPermit(Arc<Cell>);

impl Checked {
    pub(crate) fn begin_effect(&self) -> Result<EffectPermit, String> {
        let mut state = self.0.cell.state.lock().unwrap_or_else(|e| e.into_inner());
        self.0.allowed(&state)?;
        state.effects += 1;
        drop(state);
        let permit = EffectPermit(Arc::clone(&self.0.cell));
        #[cfg(test)]
        tests::point(&self.0.key, tests::Point::Authorized);
        Ok(permit)
    }
}

impl Drop for EffectPermit {
    fn drop(&mut self) {
        self.0
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .effects -= 1;
        self.0.changed.notify_waiters();
    }
}

pub(crate) struct Launching {
    scope: Arc<Scope>,
    previous: Option<Option<Arc<Scope>>>,
}

impl Launching {
    #[cfg(any(unix, test))]
    pub(crate) fn enter(owner: &HostedOwner) -> Result<Self, String> {
        let mut guard = Self::for_launch(owner)?;
        guard.previous = Some(CURRENT.with(|current| current.replace(Some(guard.scope()))));
        Ok(guard)
    }

    pub(crate) fn for_launch(owner: &HostedOwner) -> Result<Self, String> {
        let key = (
            owner.provider.clone(),
            owner.discord_token_hash.clone(),
            owner
                .channel_id
                .parse()
                .map_err(|_| "herdr clear: invalid channel")?,
        );
        let scope = match current()
            .filter(|scope| scope.key == key && !scope.finished.load(Ordering::Acquire))
        {
            Some(scope) => {
                scope.allowed(&scope.cell.state.lock().unwrap_or_else(|e| e.into_inner()))?;
                scope
            }
            None => Scope::register(key)?,
        };
        Ok(Self {
            scope,
            previous: None,
        })
    }

    pub(crate) fn scope(&self) -> Arc<Scope> {
        Arc::clone(&self.scope)
    }
}

impl Drop for Launching {
    fn drop(&mut self) {
        if let Some(previous) = self.previous.take() {
            CURRENT.with(|current| current.replace(previous));
        }
    }
}

pub(crate) fn current() -> Option<Arc<Scope>> {
    CURRENT.with(|current| current.borrow().clone())
}

#[cfg(any(unix, test))]
pub(crate) struct Current(Option<Arc<Scope>>);

/// A synchronous worker receives the originating scope explicitly, without inheriting its TLS.
#[cfg(any(unix, test))]
pub(crate) fn install(scope: Option<Arc<Scope>>) -> Current {
    Current(CURRENT.with(|current| current.replace(scope)))
}

#[cfg(any(unix, test))]
impl Drop for Current {
    fn drop(&mut self) {
        CURRENT.with(|current| current.replace(self.0.take()));
    }
}

#[cfg(test)]
pub(crate) mod tests;
