//! An input notice is classified once per prompt and injection history state: the history file
//! is the only outside state the classifier reads, so an unchanged one gives the same answer.
use crate::services::tui_input::human_frame::history::{KnownToken, known_token};
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, PoisonError};

/// A few hundred pending notices at most; a full memo starts over rather than growing.
const ENTRY_LIMIT: usize = 512;
const BYTE_LIMIT: usize = 8 << 20;

#[derive(Default)]
struct Memo {
    answers: HashMap<String, (KnownToken, Option<String>)>,
    bytes: usize,
}

static MEMO: LazyLock<Mutex<Memo>> = LazyLock::new(Mutex::default);

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    /// Classifier runs on this thread, for the cost tests.
    pub static RUNS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// `classify`'s answer for `prompt`, reused while the history keeps the state it was made under.
pub(in crate::services::discord) fn content(
    prompt: &str,
    classify: impl FnOnce(&str) -> Option<String>,
) -> Option<String> {
    let memo = || MEMO.lock().unwrap_or_else(PoisonError::into_inner);
    let before = known_token();
    if let Some(token) = &before
        && let Some((_, answer)) = memo().answers.get(prompt).filter(|(at, _)| at == token)
    {
        return answer.clone();
    }
    #[cfg(any(test, feature = "test-support"))]
    RUNS.with(|runs| runs.set(runs.get() + 1));
    let answer = classify(prompt);
    let size = prompt.len() + answer.as_ref().map_or(0, String::len);
    if let Some(token) = before
        && size <= BYTE_LIMIT / 16
        && known_token().as_ref() == Some(&token)
    {
        let mut memo = memo();
        if memo.answers.len() >= ENTRY_LIMIT || memo.bytes + size > BYTE_LIMIT {
            *memo = Memo::default();
        }
        memo.bytes += size;
        if let Some((_, old)) = memo
            .answers
            .insert(prompt.to_owned(), (token, answer.clone()))
        {
            memo.bytes -= prompt.len() + old.map_or(0, |old| old.len());
        }
    }
    answer
}
