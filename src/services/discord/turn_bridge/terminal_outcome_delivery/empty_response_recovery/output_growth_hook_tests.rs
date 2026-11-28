static PENDING: std::sync::LazyLock<dashmap::DashMap<String, String>> =
    std::sync::LazyLock::new(dashmap::DashMap::new);

pub(in crate::services::discord) fn after_first_scan(path: &str, line: &str) {
    PENDING.insert(path.to_string(), line.to_string());
}

pub(super) fn grow(path: &str) {
    use std::io::Write as _;
    let Some((_, line)) = PENDING.remove(path) else {
        return;
    };
    let mut file = std::fs::OpenOptions::new().append(true).open(path);
    writeln!(file.as_mut().unwrap(), "{line}").unwrap();
}
