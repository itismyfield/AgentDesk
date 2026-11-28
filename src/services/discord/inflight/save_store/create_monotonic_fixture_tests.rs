use super::*;
use std::io::Write;

pub(in crate::services::discord) static SERIAL: Mutex<()> = Mutex::new(());

pub(in crate::services::discord) fn state(
    channel: u64,
    path: &Path,
    start: u64,
) -> InflightTurnState {
    InflightTurnState::new(
        ProviderKind::Codex,
        channel,
        None,
        7,
        start + 10,
        9,
        "turn".into(),
        None,
        Some("AgentDesk-codex-observer-5490".into()),
        Some(path.display().to_string()),
        None,
        start,
    )
}

pub(in crate::services::discord) fn write_bytes(path: &Path, byte: u8, len: usize) {
    let mut file = fs::File::create(path).unwrap();
    file.write_all(&vec![byte; len]).unwrap();
    file.sync_all().unwrap();
}
