#[derive(Debug, Clone, Default)]
pub struct AutoQueueLogContext<'a> {
    pub run_id: Option<&'a str>,
    pub entry_id: Option<&'a str>,
    pub card_id: Option<&'a str>,
    pub dispatch_id: Option<&'a str>,
    pub thread_group: Option<i64>,
    pub batch_phase: Option<i64>,
    pub slot_index: Option<i64>,
    pub agent_id: Option<&'a str>,
}

impl<'a> AutoQueueLogContext<'a> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn run(mut self, run_id: &'a str) -> Self {
        self.run_id = Some(run_id);
        self
    }

    pub fn entry(mut self, entry_id: &'a str) -> Self {
        self.entry_id = Some(entry_id);
        self
    }

    pub fn card(mut self, card_id: &'a str) -> Self {
        self.card_id = Some(card_id);
        self
    }

    pub fn dispatch(mut self, dispatch_id: &'a str) -> Self {
        self.dispatch_id = Some(dispatch_id);
        self
    }

    pub fn maybe_dispatch(mut self, dispatch_id: Option<&'a str>) -> Self {
        self.dispatch_id = normalize_auto_queue_log_id(dispatch_id);
        self
    }

    pub fn agent(mut self, agent_id: &'a str) -> Self {
        self.agent_id = Some(agent_id);
        self
    }

    pub fn thread_group(mut self, thread_group: i64) -> Self {
        self.thread_group = Some(thread_group);
        self
    }

    pub fn batch_phase(mut self, batch_phase: i64) -> Self {
        self.batch_phase = Some(batch_phase);
        self
    }

    pub fn slot_index(mut self, slot_index: i64) -> Self {
        self.slot_index = Some(slot_index);
        self
    }

    pub fn maybe_slot_index(mut self, slot_index: Option<i64>) -> Self {
        self.slot_index = slot_index;
        self
    }
}

fn normalize_auto_queue_log_id(value: Option<&str>) -> Option<&str> {
    value.filter(|value| !value.trim().is_empty())
}
