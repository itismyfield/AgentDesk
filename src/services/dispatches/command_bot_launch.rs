use std::sync::OnceLock;

static LOAD_TOKENS: OnceLock<fn() -> Vec<String>> = OnceLock::new();

pub(crate) fn install(load_tokens: fn() -> Vec<String>) {
    let _ = LOAD_TOKENS.set(load_tokens);
}

pub(super) fn tokens() -> Vec<String> {
    LOAD_TOKENS.get().map_or_else(Vec::new, |load| load())
}
