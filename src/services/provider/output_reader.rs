//! Shared output-file polling for provider streams.

use super::{
    CancelToken, ReadOutputResult, ReadyForInputIdleState, ReadyForInputIdleTracker,
    cancel_requested,
};

#[allow(clippy::too_many_arguments)]
pub fn poll_output_file_until_result<
    State,
    IsAlive,
    IsReady,
    EmitOffset,
    ProcessLine,
    HasFinal,
    EmitSyntheticDone,
    EmitDeferredError,
>(
    output_path: &str,
    start_offset: u64,
    cancel_token: Option<std::sync::Arc<CancelToken>>,
    state: &mut State,
    mut is_alive: IsAlive,
    mut is_ready_for_input: IsReady,
    mut emit_output_offset: EmitOffset,
    mut process_line: ProcessLine,
    has_final: HasFinal,
    mut emit_synthetic_done: EmitSyntheticDone,
    mut emit_deferred_error: EmitDeferredError,
) -> Result<ReadOutputResult, String>
where
    IsAlive: FnMut() -> bool,
    IsReady: FnMut() -> bool,
    EmitOffset: FnMut(u64),
    ProcessLine: FnMut(&str, &mut State) -> bool,
    HasFinal: Fn(&State) -> bool,
    EmitSyntheticDone: FnMut(&State) -> bool,
    EmitDeferredError: FnMut(&State),
{
    use std::io::{Read, Seek, SeekFrom};
    use std::time::{Duration, Instant};

    let wait_start = Instant::now();
    let mut wait_interval = Duration::from_millis(10);
    let max_wait_interval = Duration::from_millis(500);
    loop {
        if std::fs::metadata(output_path).is_ok() {
            break;
        }
        if !is_alive() {
            return Ok(ReadOutputResult::SessionDied {
                offset: start_offset,
            });
        }
        if wait_start.elapsed() > Duration::from_secs(30) {
            return Err("Timeout waiting for output file".to_string());
        }
        if cancel_requested(cancel_token.as_deref()) {
            return Ok(ReadOutputResult::Cancelled {
                offset: start_offset,
            });
        }
        std::thread::sleep(wait_interval);
        wait_interval = std::cmp::min(
            Duration::from_millis((wait_interval.as_millis() as f64 * 1.5) as u64),
            max_wait_interval,
        );
    }

    if start_offset > 0 {
        emit_output_offset(start_offset);
    }

    let mut file = std::fs::File::open(output_path)
        .map_err(|e| format!("Failed to open output file: {}", e))?;
    file.seek(SeekFrom::Start(start_offset))
        .map_err(|e| format!("Failed to seek output file: {}", e))?;

    let mut current_offset = start_offset;
    let mut committed_offset = start_offset;
    let mut partial_line = Vec::new();
    let mut buf = [0u8; 8192];
    let mut no_data_count: u32 = 0;
    let mut ready_for_input_tracker = ReadyForInputIdleTracker::default();

    loop {
        if cancel_requested(cancel_token.as_deref()) {
            return Ok(ReadOutputResult::Cancelled {
                offset: committed_offset,
            });
        }

        match file.read(&mut buf) {
            Ok(0) => {
                if crate::services::tmux_common::rotation_target_was_swapped(
                    &file,
                    std::path::Path::new(output_path),
                )
                .map_err(|error| format!("Failed to verify output file identity: {error}"))?
                {
                    return Err("Output file rotated before a terminal result".into());
                }
                no_data_count += 1;
                if no_data_count % 25 == 0 {
                    let alive = is_alive();
                    // Growth belongs to the descriptor being read, never a replacement path.
                    let file_len = file
                        .metadata()
                        .map(|meta| meta.len())
                        .unwrap_or(current_offset);
                    if !alive {
                        if file_len > current_offset {
                            continue;
                        }
                        break;
                    }

                    let has_new_bytes = file_len > current_offset;
                    let output_ever_grew = current_offset > start_offset;
                    if !has_new_bytes
                        && ready_for_input_tracker.observe_idle_state(
                            output_ever_grew,
                            is_ready_for_input(),
                            true,
                            Instant::now(),
                        ) == ReadyForInputIdleState::PostWorkIdleTimeout
                    {
                        if !emit_synthetic_done(state) {
                            return Ok(ReadOutputResult::Cancelled {
                                offset: committed_offset,
                            });
                        }
                        return Ok(ReadOutputResult::Completed {
                            offset: committed_offset,
                        });
                    } else if has_new_bytes {
                        ready_for_input_tracker.record_output();
                    }
                }

                let read_interval = if no_data_count < 5 {
                    Duration::from_millis(10)
                } else if no_data_count < 20 {
                    Duration::from_millis(50)
                } else {
                    Duration::from_millis(200)
                };
                std::thread::sleep(read_interval);
            }
            Ok(n) => {
                no_data_count = 0;
                ready_for_input_tracker.record_output();
                current_offset += n as u64;
                partial_line.extend_from_slice(&buf[..n]);
                if let Some(pos) = partial_line.iter().rposition(|byte| *byte == b'\n') {
                    emit_output_offset(committed_offset.saturating_add((pos + 1) as u64));
                }

                while let Some(pos) = partial_line.iter().position(|byte| *byte == b'\n') {
                    let line: Vec<u8> = partial_line.drain(..=pos).collect();
                    committed_offset = committed_offset.saturating_add(line.len() as u64);
                    let line = String::from_utf8_lossy(&line);
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }

                    if !process_line(trimmed, state) {
                        return Ok(ReadOutputResult::Cancelled {
                            offset: committed_offset,
                        });
                    }

                    if has_final(state) {
                        return Ok(ReadOutputResult::Completed {
                            offset: committed_offset,
                        });
                    }
                }
            }
            Err(_) => break,
        }
    }

    emit_deferred_error(state);
    Ok(ReadOutputResult::SessionDied {
        offset: committed_offset,
    })
}
