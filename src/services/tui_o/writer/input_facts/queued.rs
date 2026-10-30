//! Each queued input Claude received, kept until the native turn that ran it closes; only a record
//! tied to that input by body, or Claude's own FIFO dequeue, moves it on.

use serde_json::Value;

use crate::services::tui_o::shadow::ShadowProvider;
use crate::services::tui_o::turn_mode::carried_close_mutant as mutant;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    /// Received (`enqueue`), not taken yet.
    Waiting,
    /// Taken by Claude's FIFO `dequeue` to open the next turn.
    Dequeued,
    /// Received as a human command outside a running turn; the next opener runs it.
    Attached,
    /// Run by the open turn; its close settles it.
    Running,
}

/// What one record says about the queue.
enum Record {
    Enqueue(String),
    /// Removed without running (`remove` other than mid-turn absorption).
    Withdraw(String),
    /// Every queued input pulled back to the prompt.
    WithdrawAll,
    Dequeue,
    /// A human prompt command (`queued_command`, `prompt` mode, human origin).
    Human(String),
    /// A user prompt row.
    Prompt(String),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct QueuedInputs(Vec<(String, Stage)>);

impl QueuedInputs {
    /// An input received whose run has not started.
    pub(super) fn owed(&self) -> bool {
        self.0.iter().any(|(_, stage)| *stage != Stage::Running)
    }

    /// Applies `value`, read after its own turn facts left the reader `open`; `opened` when this
    /// record opened the turn.
    pub(super) fn read(
        &mut self,
        provider: ShadowProvider,
        value: &Value,
        open: bool,
        opened: bool,
    ) {
        if provider != ShadowProvider::Claude {
            return;
        }
        let record = classify(value);
        if mutant("bulk") && (opened || value["type"] == "attachment") {
            self.0.clear();
            return;
        }
        match record {
            Some(Record::Enqueue(body)) => self.0.push((body, Stage::Waiting)),
            Some(Record::Withdraw(body)) => {
                if let Some(at) = self.find(&body, |stage| stage != Stage::Running) {
                    self.0.remove(at);
                }
            }
            Some(Record::WithdrawAll) => self.0.retain(|(_, stage)| *stage == Stage::Running),
            Some(Record::Dequeue) => {
                if let Some(at) = self.find_stage(Stage::Waiting) {
                    self.0[at].1 = Stage::Dequeued;
                }
            }
            Some(Record::Human(body)) if open => self.run(&body),
            Some(Record::Human(body)) => match self.find(&body, |stage| stage == Stage::Waiting) {
                Some(at) => self.0[at].1 = Stage::Attached,
                None => self.0.push((body, Stage::Attached)),
            },
            Some(Record::Prompt(body)) if open => self.run(&body),
            Some(Record::Prompt(_)) | None => {}
        }
        if opened {
            for (_, stage) in &mut self.0 {
                if matches!(stage, Stage::Dequeued | Stage::Attached) {
                    *stage = Stage::Running;
                }
            }
        }
    }

    /// The open turn closed: what it ran is settled.
    pub(super) fn closed(&mut self) {
        self.0.retain(|(_, stage)| *stage != Stage::Running);
    }

    fn run(&mut self, body: &str) {
        if let Some(at) = self.find(body, |stage| stage != Stage::Running) {
            self.0[at].1 = Stage::Running;
        }
    }

    fn find(&self, body: &str, stage: impl Fn(Stage) -> bool) -> Option<usize> {
        self.0
            .iter()
            .position(|(queued, at)| queued == body && stage(*at))
    }

    fn find_stage(&self, stage: Stage) -> Option<usize> {
        self.0.iter().position(|(_, at)| *at == stage)
    }
}

fn classify(value: &Value) -> Option<Record> {
    let text = |key: &str| value.get(key).and_then(body);
    match value["type"].as_str()? {
        "queue-operation" => match value["operation"].as_str()? {
            "enqueue" => Some(Record::Enqueue(text("content")?)),
            "remove" if value["reason"] == "absorbed_mid_turn" => None,
            "remove" => Some(Record::Withdraw(text("content")?)),
            "popAll" => Some(Record::WithdrawAll),
            "dequeue" => Some(Record::Dequeue),
            _ => None,
        },
        "attachment" => {
            let command = &value["attachment"];
            let human = command["type"] == "queued_command"
                && command["commandMode"] == "prompt"
                && command["origin"]["kind"] == "human";
            human
                .then(|| body(&command["prompt"]))
                .flatten()
                .map(Record::Human)
        }
        "user" if value["isMeta"] != Value::Bool(true) => {
            body(&value["message"]["content"]).map(Record::Prompt)
        }
        _ => None,
    }
}

/// A body as Claude records it: a string, or its text blocks joined.
fn body(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.trim().to_string()),
        Value::Array(blocks) => {
            let texts: Vec<&str> = blocks
                .iter()
                .filter(|block| block["type"] == "text")
                .filter_map(|block| block["text"].as_str())
                .collect();
            (!texts.is_empty()).then(|| texts.join("\n").trim().to_string())
        }
        _ => None,
    }
}
