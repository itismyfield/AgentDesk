//! Each queued input Claude received, kept until the native turn that ran it closes; only a record
//! tied to that input by body, or Claude's own FIFO dequeue, moves it on.

use super::native::{QueueRecord as Record, classify};
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
