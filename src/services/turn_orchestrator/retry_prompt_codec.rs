use serde::{Deserialize, Serialize};

const PREFIX: &str = "\u{1e}agentdesk.retry-prompt.v1\n";

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Preludes {
    pub(crate) recovery: Option<String>,
    pub(crate) feedback: Option<String>,
    pub(crate) wip: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RetryPromptContext {
    version: u8,
    legacy_rendered: bool,
    pub(crate) raw: Option<String>,
    pub(crate) selected: Preludes,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) expected_prompt: Option<String>,
}

impl RetryPromptContext {
    pub(crate) fn raw(raw: Option<String>) -> Self {
        Self {
            version: 1,
            legacy_rendered: false,
            raw,
            selected: Preludes::default(),
            expected_prompt: None,
        }
    }

    pub(crate) fn for_routing(reply: Option<String>, queued: bool) -> Option<String> {
        let context = if queued {
            Self::restore(reply)
        } else {
            Self::raw(reply)
        };
        context.requeue()
    }

    pub(crate) fn restore(value: Option<String>) -> Self {
        if value.is_none() {
            return Self::raw(None);
        }
        Self::decode(value.as_deref()).unwrap_or(Self {
            legacy_rendered: true,
            ..Self::raw(value)
        })
    }

    pub(crate) fn decode(value: Option<&str>) -> Option<Self> {
        let context: Self = serde_json::from_str(value?.strip_prefix(PREFIX)?).ok()?;
        (context.version == 1).then_some(context)
    }

    pub(crate) fn encode(&self) -> Option<String> {
        serde_json::to_string(self)
            .ok()
            .map(|json| format!("{PREFIX}{json}"))
    }

    pub(crate) fn may_select(&self) -> bool {
        !self.legacy_rendered
    }

    pub(crate) fn requeue(&self) -> Option<String> {
        // Selection only borrows the channel stash until confirmed input; handback consumes nothing.
        Self {
            selected: Preludes::default(),
            expected_prompt: None,
            ..self.clone()
        }
        .encode()
    }
}
