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
