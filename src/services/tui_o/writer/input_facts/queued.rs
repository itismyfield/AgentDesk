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
    /// Taken into the running turn (`remove`, `absorbed_mid_turn`); its command follows.
    Absorbed,
    /// A human prompt command (`queued_command`, `prompt` mode, human origin).
    Human(String),
    /// Any other `queued_command`, such as a background task's notification drained mid-turn.
    Command(String),
    /// A user prompt row, or a meta row (a notification opening its own turn).
    Prompt(String),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct QueuedInputs {
    inputs: Vec<(String, Stage)>,
    /// Removals whose own `dequeue` record is still to come; that dequeue takes no other input.
    removed: usize,
}

impl QueuedInputs {
    /// How many received inputs have not started their run.
    pub(super) fn owed(&self) -> usize {
        let waiting = self
            .inputs
            .iter()
            .filter(|(_, stage)| *stage != Stage::Running);
        waiting.count()
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
            self.inputs.clear();
            return;
        }
        match record {
            Some(Record::Enqueue(body)) => self.inputs.push((body, Stage::Waiting)),
            Some(Record::Withdraw(body)) => {
                self.removed += 1;
                if let Some(at) = self.find(&body, |stage| stage != Stage::Running) {
                    self.inputs.remove(at);
                }
            }
            Some(Record::Absorbed) => self.removed += 1,
            Some(Record::WithdrawAll) => self.inputs.retain(|(_, stage)| *stage == Stage::Running),
            Some(Record::Dequeue) if self.removed > 0 && !mutant("dequeue_credit") => {
                self.removed -= 1
            }
            Some(Record::Dequeue) => {
                if let Some(at) = self.find_stage(Stage::Waiting) {
                    self.inputs[at].1 = Stage::Dequeued;
                }
            }
            Some(Record::Human(body)) if open => self.run(&body),
            Some(Record::Human(body)) => match self.find(&body, |stage| stage == Stage::Waiting) {
                Some(at) => self.inputs[at].1 = Stage::Attached,
                None => self.inputs.push((body, Stage::Attached)),
            },
            Some(Record::Command(body) | Record::Prompt(body)) if open => self.run(&body),
            Some(Record::Command(_) | Record::Prompt(_)) | None => {}
        }
        if opened {
            for (_, stage) in &mut self.inputs {
                if matches!(stage, Stage::Dequeued | Stage::Attached) {
                    *stage = Stage::Running;
                }
            }
        }
    }

    /// The open turn closed: what it ran is settled.
    pub(super) fn closed(&mut self) {
        self.inputs.retain(|(_, stage)| *stage != Stage::Running);
    }

    fn run(&mut self, body: &str) {
        if let Some(at) = self.find(body, |stage| stage != Stage::Running) {
            self.inputs[at].1 = Stage::Running;
        }
    }

    fn find(&self, body: &str, stage: impl Fn(Stage) -> bool) -> Option<usize> {
        self.inputs
            .iter()
            .position(|(queued, at)| queued == body && stage(*at))
    }

    fn find_stage(&self, stage: Stage) -> Option<usize> {
        self.inputs.iter().position(|(_, at)| *at == stage)
    }
}

fn classify(value: &Value) -> Option<Record> {
    let text = |key: &str| value.get(key).and_then(body);
    match value["type"].as_str()? {
        "queue-operation" => match value["operation"].as_str()? {
            "enqueue" => Some(Record::Enqueue(text("content")?)),
            "remove" if value["reason"] == "absorbed_mid_turn" => Some(Record::Absorbed),
            "remove" => Some(Record::Withdraw(text("content")?)),
            "popAll" => Some(Record::WithdrawAll),
            "dequeue" => Some(Record::Dequeue),
            _ => None,
        },
        "attachment" if value["attachment"]["type"] == "queued_command" => {
            let command = &value["attachment"];
            let human = command["commandMode"] == "prompt" && command["origin"]["kind"] == "human";
            let prompt = body(&command["prompt"])?;
            match human {
                true => Some(Record::Human(prompt)),
                false if mutant("notification_unlinked") => None,
                false => Some(Record::Command(prompt)),
            }
        }
        "user" => body(&value["message"]["content"]).map(Record::Prompt),
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
