use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

#[derive(Default)]
struct State {
    sealed: bool,
    #[cfg(all(test, unix))]
    published: usize,
    #[cfg(all(test, unix))]
    received: usize,
    #[cfg(all(test, unix))]
    producers: usize,
}
struct Gate {
    state: Mutex<State>,
    #[cfg(all(test, unix))]
    changed: std::sync::Condvar,
}

pub(crate) struct Sender<T> {
    inner: mpsc::Sender<T>,
    gate: Arc<Gate>,
}
pub(crate) struct Receiver<T> {
    inner: mpsc::Receiver<T>,
    gate: Arc<Gate>,
}
pub(crate) fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let (tx, rx) = mpsc::channel();
    let gate = Arc::new(Gate {
        state: Mutex::new(State::default()),
        #[cfg(all(test, unix))]
        changed: std::sync::Condvar::new(),
    });
    #[cfg(all(test, unix))]
    {
        gate.state.lock().unwrap().producers = 1;
        test_delay::register_gate(gate.clone());
    }
    (
        Sender {
            inner: tx,
            gate: gate.clone(),
        },
        Receiver { inner: rx, gate },
    )
}
impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        #[cfg(all(test, unix))]
        {
            self.gate.state.lock().unwrap().producers += 1;
        }
        Self {
            inner: self.inner.clone(),
            gate: self.gate.clone(),
        }
    }
}
impl<T> Sender<T> {
    pub(crate) fn send(&self, value: T) -> Result<(), mpsc::SendError<T>> {
        let state = self.gate.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.sealed {
            return Err(mpsc::SendError(value));
        }
        let result = self.inner.send(value);
        #[cfg(all(test, unix))]
        {
            let mut state = state;
            if result.is_ok() {
                state.published += 1;
            }
        }
        result
    }
}
#[cfg(all(test, unix))]
impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        let mut state = self.gate.state.lock().unwrap_or_else(|e| e.into_inner());
        state.producers -= 1;
        self.gate.changed.notify_all();
    }
}
impl<T> Receiver<T> {
    pub(crate) fn seal(&self) {
        self.gate
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .sealed = true;
    }
    pub(crate) fn recv_timeout(&self, timeout: Duration) -> Result<T, mpsc::RecvTimeoutError> {
        #[cfg(all(test, unix))]
        test_delay::before_receive(&self.gate);
        let sealed = self
            .gate
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .sealed;
        // Seal serializes with every send; the remaining queue cannot keep growing.
        let result = if sealed {
            self.inner
                .try_recv()
                .map_err(|_| mpsc::RecvTimeoutError::Disconnected)
        } else {
            self.inner.recv_timeout(timeout)
        };
        #[cfg(all(test, unix))]
        if result.is_ok() {
            self.gate.state.lock().unwrap().received += 1;
        }
        result
    }
}

#[cfg(all(test, unix))]
pub(crate) mod test_delay;
#[cfg(test)]
mod tests;
