//! Each queued input Claude received, kept until the native turn that ran it closes; only a record
//! tied to that input by body, or Claude's own FIFO dequeue, moves it on.

use serde_json::Value;

use crate::services::tui_o::shadow::ShadowProvider;

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
pub(crate) enum Record {
    Enqueue(String),
    /// Removed without running (`remove` other than mid-turn absorption).
    Withdraw(String),
    /// Handed to another agent (`remove`, `delivered_to_agent`); Claude writes no dequeue for it.
    Delivered(String),
    /// Every queued input pulled back to the prompt.
    WithdrawAll,
    Dequeue,
    /// Taken into the turn (`remove`, `absorbed_mid_turn`); Claude writes no dequeue for it, and its
    /// `queued_command` row may come before it, after it, or not at all.
    Absorbed(String),
    /// A human prompt command (`queued_command`, `prompt` mode, human origin).
    Human(String),
    /// Any other `queued_command`, such as a background task's notification drained mid-turn.
    Command(String),
    /// A user prompt row, or a meta row (a notification opening its own turn).
    Prompt(String),
}

/// One received input, with which of its own command and absorption records were read.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Input {
    body: String,
    stage: Stage,
    command: bool,
    absorbed: bool,
}

impl Input {
    fn new(body: String, stage: Stage) -> Self {
        Self {
            body,
            stage,
            command: false,
            absorbed: false,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct QueuedInputs {
    inputs: Vec<Input>,
    /// Removals whose own `dequeue` record is still to come; that dequeue takes no other input.
    removed: usize,
}

impl QueuedInputs {
    /// How many received inputs have not started their run.
    pub(super) fn owed(&self) -> usize {
        let waiting = self
            .inputs
            .iter()
            .filter(|input| input.stage != Stage::Running);
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
        match record {
            Some(Record::Enqueue(body)) => self.inputs.push(Input::new(body, Stage::Waiting)),
            Some(Record::Withdraw(body)) => {
                self.removed += 1;
                self.withdraw(&body);
            }
            Some(Record::Delivered(body)) => self.withdraw(&body),
            Some(Record::Absorbed(body)) => self.absorb(&body, open),
            Some(Record::WithdrawAll) => self.inputs.retain(|input| input.stage == Stage::Running),
            Some(Record::Dequeue) if self.removed > 0 => self.removed -= 1,
            Some(Record::Dequeue) => {
                if let Some(at) = self.find(|input| input.stage == Stage::Waiting) {
                    self.inputs[at].stage = Stage::Dequeued;
                }
            }
            // A command row outside a turn first pairs with an absorption read before it.
            Some(Record::Human(body) | Record::Command(body))
                if !open && self.paired_command(&body) => {}
            Some(Record::Human(body) | Record::Command(body)) if open => self.command(&body),
            Some(Record::Human(body)) => {
                let mut input = Input::new(body, Stage::Attached);
                input.command = true;
                match self.find(|i| i.body == input.body && i.stage == Stage::Waiting) {
                    Some(at) => self.inputs[at] = input,
                    None => self.inputs.push(input),
                }
            }
            Some(Record::Command(_)) => {}
            Some(Record::Prompt(body)) if open => self.run(&body),
            Some(Record::Prompt(_)) | None => {}
        }
        if opened {
            for input in &mut self.inputs {
                if matches!(input.stage, Stage::Dequeued | Stage::Attached) {
                    input.stage = Stage::Running;
                }
            }
        }
    }

    /// The open turn closed: what it ran is settled.
    pub(super) fn closed(&mut self) {
        self.inputs.retain(|input| input.stage != Stage::Running);
    }

    fn withdraw(&mut self, body: &str) {
        if let Some(at) = self.find(|i| i.body == body && i.stage != Stage::Running) {
            self.inputs.remove(at);
        }
    }

    /// An absorption is its input's run: the open turn runs it, or the turn it opens after a close.
    /// An input whose command row already ran it only records the absorption.
    fn absorb(&mut self, body: &str, open: bool) {
        let ran =
            |i: &Input| i.body == body && i.command && !i.absorbed && i.stage != Stage::Waiting;
        if let Some(at) = self.find(ran) {
            self.inputs[at].absorbed = true;
            return;
        }
        let taken = |i: &Input| i.body == body && !i.absorbed && i.stage != Stage::Running;
        if let Some(at) = self.find(taken) {
            let input = &mut self.inputs[at];
            input.absorbed = true;
            input.stage = if open {
                Stage::Running
            } else {
                Stage::Attached
            };
        }
    }

    /// A command row in the open turn runs its input, unless its absorption already did.
    fn command(&mut self, body: &str) {
        if self.paired_command(body) {
            return;
        }
        if let Some(at) = self.find(|i| i.body == body && i.stage != Stage::Running) {
            self.inputs[at].stage = Stage::Running;
            self.inputs[at].command = true;
        }
    }

    /// Records the command of an input whose absorption was read before it.
    fn paired_command(&mut self, body: &str) -> bool {
        match self.find(|i| i.body == body && i.absorbed && !i.command) {
            Some(at) => {
                self.inputs[at].command = true;
                true
            }
            None => false,
        }
    }

    fn run(&mut self, body: &str) {
        if let Some(at) = self.find(|i| i.body == body && i.stage != Stage::Running) {
            self.inputs[at].stage = Stage::Running;
        }
    }

    fn find(&self, matches: impl Fn(&Input) -> bool) -> Option<usize> {
        self.inputs.iter().position(matches)
    }
}

pub(crate) fn classify(value: &Value) -> Option<Record> {
    let text = |key: &str| value.get(key).and_then(body);
    match value["type"].as_str()? {
        "queue-operation" => match value["operation"].as_str()? {
            "enqueue" => Some(Record::Enqueue(text("content")?)),
            "remove" if value["reason"] == "absorbed_mid_turn" => {
                Some(Record::Absorbed(text("content")?))
            }
            "remove" if value["reason"] == "delivered_to_agent" => {
                Some(Record::Delivered(text("content")?))
            }
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
