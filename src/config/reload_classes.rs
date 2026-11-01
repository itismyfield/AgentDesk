//! When an `agentdesk.yaml` edit takes effect, per field. The hot-reload
//! restart list and `docs/config-reload-classes.md` are both derived from [`FIELDS`].

use std::collections::BTreeMap;

use serde_json::Value;

use super::Config;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReloadClass {
    /// Read from the live snapshot (or disk) at use time.
    Live,
    /// Captured at boot or on first access (OnceLock, long-lived task, global setter).
    Restart,
    /// Copied into the DB/kv at boot; runtime readers (Rust or policy JS) read the copy.
    SeededAtBoot,
}

impl ReloadClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Restart => "restart",
            Self::SeededAtBoot => "seeded-at-boot",
        }
    }
}

#[derive(Clone, Copy)]
pub enum Detect {
    /// The serialized value at `path` differs; `*` spans every map key or list index.
    Value,
    /// A secret or boot-relative comparison the serialized snapshot cannot express.
    Custom(fn(&Config, &Config) -> bool),
    /// Covered by a neighbouring row's custom detector (see `detect.rs`).
    Covered,
}

pub struct FieldReload {
    /// Dotted YAML key path; `*` is a map key or list item.
    pub path: &'static str,
    pub class: ReloadClass,
    pub detect: Detect,
    /// `file` + symbol that caches, seeds, or reads the value at use time.
    pub reader: &'static str,
}

pub(super) const fn row(
    path: &'static str,
    class: ReloadClass,
    reader: &'static str,
) -> FieldReload {
    FieldReload {
        path,
        class,
        detect: Detect::Value,
        reader,
    }
}

pub(super) const fn custom(
    path: &'static str,
    class: ReloadClass,
    detect: Detect,
    reader: &'static str,
) -> FieldReload {
    FieldReload {
        path,
        class,
        detect,
        reader,
    }
}

mod detect;
mod table;
pub use table::{FIELDS, FILES};

use ReloadClass::Live;

#[cfg(test)]
mod tests;

/// Non-live rows whose value differs between two snapshots, in table order.
pub fn changed_non_live(old: &Config, new: &Config) -> Vec<&'static FieldReload> {
    let values = snapshot(old).and_then(|old| Ok((old, snapshot(new)?)));
    FIELDS
        .iter()
        .filter(|field| field.class != Live)
        .filter(|field| match (field.detect, &values) {
            (Detect::Value, Ok((old_value, new_value))) => {
                values_at(old_value, field.path) != values_at(new_value, field.path)
            }
            // An unserializable snapshot conservatively counts as changed.
            (Detect::Value, Err(_)) => true,
            (Detect::Custom(changed), _) => changed(old, new),
            (Detect::Covered, _) => false,
        })
        .collect()
}

/// Called by the policy watcher: a restart-class file edit is not applied by the reload.
pub fn warn_restart_files(paths: &[std::path::PathBuf]) {
    for file in FILES {
        let name = std::path::Path::new(file.path).file_name();
        if paths.iter().any(|path| path.file_name() == name) {
            tracing::warn!(
                file = file.path,
                class = file.class.as_str(),
                captured_by = file.reader,
                "policy hot-reload saw an edit to a file read once at boot; restart to apply it"
            );
        }
    }
}

pub fn class_of(path: &str) -> Option<ReloadClass> {
    FIELDS
        .iter()
        .chain(FILES)
        .find(|field| field.path == path)
        .map(|field| field.class)
}

/// Serialized config with the sections that `skip_serializing_if` drops at their
/// default written out, so a default section compares equal to an absent one.
fn snapshot(config: &Config) -> serde_json::Result<Value> {
    let mut root = serde_json::to_value(config)?;
    let mut put = |path: &[&str], value: Value| {
        let (last, parents) = path.split_last().expect("non-empty path");
        let mut at = &mut root;
        for key in parents {
            at = &mut at[*key];
        }
        at[*last] = value;
    };
    put(&["cluster"], serde_json::to_value(&config.cluster)?);
    let routing = &config.cluster;
    put(
        &["cluster", "dispatch_routing"],
        serde_json::to_value(&routing.dispatch_routing)?,
    );
    put(
        &["cluster", "intake_routing"],
        serde_json::to_value(&routing.intake_routing)?,
    );
    put(&["kanban"], serde_json::to_value(&config.kanban)?);
    put(&["review"], serde_json::to_value(&config.review)?);
    put(&["runtime"], serde_json::to_value(&config.runtime)?);
    put(&["routines"], serde_json::to_value(&config.routines)?);
    put(&["onboarding"], serde_json::to_value(&config.onboarding)?);
    let tui_o = config.tui_o.clone().unwrap_or_default();
    put(&["tui_o"], serde_json::to_value(&tui_o)?);
    Ok(root)
}

fn values_at(root: &Value, path: &str) -> BTreeMap<String, Value> {
    let mut found = BTreeMap::new();
    collect(
        root,
        &path.split('.').collect::<Vec<_>>(),
        String::new(),
        &mut found,
    );
    found
}

fn collect(value: &Value, segments: &[&str], at: String, found: &mut BTreeMap<String, Value>) {
    let Some((head, rest)) = segments.split_first() else {
        found.insert(at, value.clone());
        return;
    };
    let mut descend = |key: String, child: &Value| {
        collect(child, rest, format!("{at}.{key}"), found);
    };
    match (value, *head) {
        (Value::Object(map), "*") => map.iter().for_each(|(k, v)| descend(k.clone(), v)),
        (Value::Array(items), "*") => items
            .iter()
            .enumerate()
            .for_each(|(i, v)| descend(i.to_string(), v)),
        (Value::Object(map), key) => {
            if let Some(child) = map.get(key) {
                descend(key.to_string(), child);
            }
        }
        // A scalar where the path goes deeper (a legacy string channel) compares whole.
        (Value::Array(_), _) | (Value::Null, _) => {}
        (scalar, _) => {
            found.insert(at, scalar.clone());
        }
    }
}
