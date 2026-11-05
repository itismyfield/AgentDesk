use serde_json::{Value, json};

pub(super) struct Window {
    pub(super) boot_id: String,
    pub(super) cut: u64,
    pub(super) drained: Option<u64>,
    pub(super) closed: bool,
    pub(super) invalid: bool,
}

impl Window {
    pub(super) fn new(boot_id: String) -> Self {
        Self {
            boot_id,
            cut: 0,
            drained: None,
            closed: false,
            invalid: false,
        }
    }

    pub(super) fn marker(&self, cut: u64, closed: bool) -> Value {
        json!({"kind":"drain","boot_id":self.boot_id,"cut":cut,
            "closed":closed,"invalid":self.invalid})
    }

    pub(super) fn observe(&mut self, record: &Value) {
        if record["kind"] == "drain" {
            if record["boot_id"] != self.boot_id
                || record["cut"].as_u64() != Some(self.cut)
                || record["closed"].as_bool().is_none()
                || record["invalid"] != false
            {
                self.invalid = true;
                return;
            }
            self.drained = Some(self.cut);
            self.closed |= record["closed"] == true;
        } else if record["kind"] == "observation_invalid" {
            self.invalid = true;
        } else {
            self.invalid |= self.closed || record["kind"].as_str().is_none();
            if let Some(next) = self.cut.checked_add(1) {
                self.cut = next;
            } else {
                self.invalid = true;
            }
        }
    }

    pub(super) fn verdict(&self, live_boot: Option<&str>, candidate: &'static str) -> &'static str {
        if !self.invalid
            && self.drained == Some(self.cut)
            && (self.closed || live_boot == Some(self.boot_id.as_str()))
        {
            candidate
        } else {
            "INCONCLUSIVE"
        }
    }

    pub(super) fn report(&self, live_boot: Option<&str>) -> Value {
        json!({"boot_id":self.boot_id,"cut":self.cut,"drained":self.drained,
            "closed":self.closed,"invalid":self.invalid,
            "status":self.verdict(live_boot,"COMPLETE")})
    }
}

// Structural evidence only; the collector must also reconcile independent boot inventory.
pub(super) fn valid_chain(
    headers: &std::collections::BTreeMap<String, Value>,
    anchor: &Value,
    end_ms: i64,
    boundary: Option<&Value>,
    visits: &mut usize,
) -> bool {
    use std::collections::BTreeSet;
    let Some(epoch) = anchor["boot_id"]
        .as_str()
        .filter(|id| uuid::Uuid::parse_str(id).is_ok())
    else {
        return false;
    };
    let Some(start_ms) = anchor["started_at_ms"].as_i64() else {
        return false;
    };
    if end_ms < start_ms
        || headers
            .get(epoch)
            .is_none_or(|header| header["started_at_ms"].as_i64() != Some(start_ms))
    {
        return false;
    }
    let mut complete = BTreeSet::new();
    let mut successors = BTreeSet::new();
    for start in headers.keys() {
        let mut path = BTreeSet::new();
        let mut cursor = Some(start.as_str());
        while let Some(id) = cursor {
            *visits += 1;
            if complete.contains(id) {
                break;
            }
            if !path.insert(id) {
                return false;
            }
            let Some(header) = headers.get(id) else {
                return false;
            };
            let Some(at) = header["started_at_ms"].as_i64() else {
                return false;
            };
            if header["boot_id"] != id
                || uuid::Uuid::parse_str(id).is_err()
                || header["coverage_start"] != *anchor
                || at < start_ms
                || at > end_ms
            {
                return false;
            }
            cursor = match header.get("predecessor_boot_id") {
                Some(Value::Null) if id == epoch => None,
                Some(Value::String(parent)) if uuid::Uuid::parse_str(parent).is_ok() => {
                    if !successors.insert(parent) {
                        return false;
                    }
                    if let Some(previous) = headers.get(parent) {
                        if previous["started_at_ms"]
                            .as_i64()
                            .is_none_or(|prior| prior > at)
                        {
                            return false;
                        }
                        Some(parent)
                    } else if id == epoch
                        && boundary.is_some_and(|previous| {
                            previous["boot_id"] == *parent
                                && previous["started_at_ms"]
                                    .as_i64()
                                    .is_some_and(|prior| prior < start_ms)
                        })
                    {
                        None
                    } else {
                        return false;
                    }
                }
                _ => return false,
            };
        }
        complete.extend(path);
    }
    true
}
