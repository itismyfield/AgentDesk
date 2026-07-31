use anyhow::Error;

pub(super) fn runtime_unavailable(error: &Error) -> bool {
    let message = error.to_string();
    [
        "provider runtime not registered",
        "provider runtime is not ready",
        "matched runtime is not ready",
        "provider token unavailable",
    ]
    .into_iter()
    .any(|needle| message.contains(needle))
}
