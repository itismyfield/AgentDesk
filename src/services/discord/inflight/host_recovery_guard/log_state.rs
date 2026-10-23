use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use crate::services::session_host::TargetHost;

type TargetKey = (String, u64, String, Option<String>);
type Key = (TargetKey, String);
type State = (String, bool, TargetHost);

static LOGGED: LazyLock<Mutex<HashMap<Key, State>>> = LazyLock::new(Default::default);

/// A successful clear re-arms every caller for this target; repeated refusals stay quiet.
pub(super) fn changed(scope: (&str, u64, &str, &str, Option<&str>), state: Option<State>) -> bool {
    let key = (
        (
            scope.0.to_owned(),
            scope.1,
            scope.3.to_owned(),
            scope.4.map(str::to_owned),
        ),
        scope.2.to_owned(),
    );
    let mut logged = LOGGED.lock().unwrap_or_else(|e| e.into_inner());
    match state {
        Some(state) if logged.get(&key) != Some(&state) => {
            logged.insert(key, state);
            true
        }
        Some(_) => false,
        None => {
            logged.retain(|(target, _), _| target != &key.0);
            false
        }
    }
}
