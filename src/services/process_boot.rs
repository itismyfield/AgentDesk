use std::sync::OnceLock;

#[derive(Clone, Debug)]
pub(crate) struct Boot {
    pub(crate) id: String,
    pub(crate) started_at_ms: i64,
    pub(crate) monotonic: std::time::Instant,
}

pub(crate) fn initialize() -> &'static Boot {
    static BOOT: OnceLock<Boot> = OnceLock::new();
    BOOT.get_or_init(|| Boot {
        monotonic: std::time::Instant::now(),
        id: uuid::Uuid::new_v4().to_string(),
        started_at_ms: chrono::Utc::now().timestamp_millis(),
    })
}
