use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use crate::services::session_host::{GuardVerdict, TargetHost};

type Key = (String, u64, String, String, Option<String>);
type State = (GuardVerdict, bool, TargetHost);

static LOGGED: LazyLock<Mutex<HashMap<Key, State>>> = LazyLock::new(Default::default);

/// A successful clear re-arms this recovery scope; repeated refusals stay quiet.
pub(super) fn changed(scope: (&str, u64, &str, &str, Option<&str>), state: Option<State>) -> bool {
    let key = (
        scope.0.to_owned(),
        scope.1,
        scope.2.to_owned(),
        scope.3.to_owned(),
        scope.4.map(str::to_owned),
    );
    let mut logged = LOGGED.lock().unwrap_or_else(|e| e.into_inner());
    match state {
        Some(state) if logged.get(&key) != Some(&state) => {
            logged.insert(key, state);
            true
        }
        Some(_) => false,
        None => {
            logged.remove(&key);
            false
        }
    }
}
