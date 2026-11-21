use serde_json::Value;
use uuid::Uuid;

#[derive(PartialEq, Eq)]
struct Terminal {
    conversation: Uuid,
    status: String,
    response: Option<String>,
}

/// Validates the capture independently of the legacy stream's terminal result.
#[derive(Default)]
pub(super) struct Capture {
    expected: Option<Uuid>,
    observed_init: Option<Uuid>,
    terminal: Option<Terminal>,
    invalid: bool,
}

impl Capture {
    pub(super) fn new(expected: Option<Uuid>) -> Self {
        Self {
            expected,
            ..Self::default()
        }
    }

    pub(super) fn invalidate(&mut self) {
        self.invalid = true;
    }

    pub(super) fn observe(&mut self, event: &str, payload: &Value, envelope: &Value) {
        let valid = match event {
            "init" => {
                let id = payload
                    .get("conversation_id")
                    .or_else(|| envelope.get("conversation_id"));
                id.and_then(conversation).is_some_and(|id| {
                    let valid = self.matches(id);
                    self.observed_init = Some(id);
                    valid
                })
            }
            "result" => self.observe_result(payload).is_some(),
            _ => true,
        };
        self.invalid |= !valid;
    }

    fn matches(&self, id: Uuid) -> bool {
        self.expected.is_none_or(|expected| expected == id)
            && self.observed_init.is_none_or(|observed| observed == id)
    }

    fn observe_result(&mut self, payload: &Value) -> Option<()> {
        let id = match payload.get("conversation_id") {
            Some(value) => conversation(value)?,
            None => self.observed_init?,
        };
        if !self.matches(id) {
            return None;
        }
        let result = Terminal {
            conversation: id,
            status: payload.get("status")?.as_str()?.to_ascii_uppercase(),
            response: match payload.get("response") {
                Some(value) => Some(value.as_str()?.to_owned()),
                None => None,
            },
        };
        if let Some(terminal) = &self.terminal {
            return (terminal == &result).then_some(());
        }
        self.terminal = Some(result);
        Some(())
    }

    pub(super) fn body(&mut self, fallback: &str) -> Option<(Uuid, String)> {
        let terminal = self.terminal.take()?;
        if self.invalid || terminal.status != "SUCCESS" {
            return None;
        }
        let body = terminal.response.unwrap_or_else(|| fallback.to_owned());
        (!body.trim().is_empty()).then_some((terminal.conversation, body))
    }
}

fn conversation(value: &Value) -> Option<Uuid> {
    Uuid::parse_str(value.as_str()?).ok()
}
