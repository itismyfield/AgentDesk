#[cfg(test)]
mod tests {
    use super::super::*;
    use crate::services::tui_o::shadow::binding_reader::source_id_for;
    use crate::services::tui_o::shadow::capture::io_trace;
    #[cfg(unix)]
    use crate::services::tui_o::shadow::capture::renumber;
    use std::io::Write;
    use std::io::{Seek, SeekFrom};

    fn fixture(bytes: &[u8], offset: u64) -> (tempfile::TempDir, ObservationCapture) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("parent.jsonl");
        std::fs::write(&path, bytes).unwrap();
        let capture =
            ObservationCapture::open(source_id_for("parent", &path).unwrap(), offset).unwrap();
        (dir, capture)
    }
    fn records(c: &mut ObservationCapture, max: u64) -> Result<Vec<CapturedRecord>, Kind> {
        match c.poll(max) {
            CaptureOutcome::Batch(b) => Ok(b.records),
            CaptureOutcome::Anomaly(a) => Err(a.kind),
        }
    }

    #[test]
    fn s1_capture_shrink_ahead_of_cursor_is_not_current_eof() {
        let bytes = b"{}\n".repeat(3000);
        let (_dir, mut capture) = fixture(&bytes, 0);
        assert_eq!(records(&mut capture, 3).unwrap().len(), 1);
        let cursor = capture.read_through();
        let smaller = 4500;
        assert!(cursor < smaller && smaller < bytes.len() as u64);
        File::options()
            .write(true)
            .open(&capture.source().path)
            .unwrap()
            .set_len(smaller)
            .unwrap();
        assert!(matches!(records(&mut capture, 9999), Err(Kind::Shrunk)));
    }

    #[test]
    fn s0o_alignment_preserves_complete_first_and_partial_last() {
        let bytes = "discard é fragment\n{}\npartial".as_bytes();
        let lf = bytes.iter().position(|b| *b == b'\n').unwrap() as u64 + 1;
        for offset in [0, 9, lf] {
            let (_dir, mut c) = fixture(bytes, offset);
            let mut seen = Vec::new();
            for _ in 0..bytes.len() {
                seen.extend(records(&mut c, 1).unwrap());
            }
            assert_eq!(seen.last().unwrap().line, b"{}");
            assert_eq!(seen.len(), if offset == 0 { 2 } else { 1 });
            assert_eq!(c.captured_through(), lf + 3);
            assert_eq!(c.read_through(), bytes.len() as u64);
            std::fs::OpenOptions::new()
                .append(true)
                .open(&c.source().path)
                .unwrap()
                .write_all(b"\n")
                .unwrap();
            assert_eq!(records(&mut c, 1).unwrap()[0].line, b"partial");
        }
        let (_dir, mut c) = fixture(b"no newline", 3);
        assert!(records(&mut c, 99).unwrap().is_empty());
        assert_eq!(c.read_through(), 10);
        assert_eq!(c.captured_through(), 10);
    }

    #[test]
    fn s0o_io_ranges_stream_actual_32mib_without_prefix_hash() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("huge.jsonl");
        let mut file = File::create(&path).unwrap();
        let end = 300 * 1024 * 1024u64;
        let window = 32 * 1024 * 1024u64;
        let start = end - window;
        file.set_len(end).unwrap();
        file.seek(SeekFrom::Start(end - 8)).unwrap();
        file.write_all(b"\n{}\nlast").unwrap();
        io_trace::take();
        let at = std::time::Instant::now();
        let mut c =
            ObservationCapture::open(source_id_for("parent", &path).unwrap(), start).unwrap();
        let mut emitted = Vec::new();
        for _ in 0..5000 {
            emitted.extend(records(&mut c, MAX_PARTIAL_BYTES as u64 + 1).unwrap());
            if c.read_through() == end {
                break;
            }
        }
        let ranges = io_trace::take();
        assert!(
            ranges
                .iter()
                .all(|(a, b, hash)| !hash && *a >= start - TAIL_GUARD_BYTES && *b <= end),
            "{ranges:?}"
        );
        assert_eq!(emitted.len(), 1);
        assert_eq!(emitted[0].line, b"{}");
        assert_eq!(c.read_through(), end);
        println!(
            "U7 window={window} elapsed_us={} ranges={ranges:?}",
            at.elapsed().as_micros()
        );
    }

    #[cfg(unix)]
    #[test]
    fn s0o_file_errors_are_sticky_and_partial_is_revalidated() {
        for mode in ["shrink", "guard", "partial", "replace"] {
            let (_dir, mut c) = fixture(&vec![b'a'; 8192], 0);
            records(&mut c, 8192).unwrap();
            let path = c.source().path.clone();
            let mut bytes = std::fs::read(&path).unwrap();
            let expected = match mode {
                "shrink" => {
                    File::options()
                        .write(true)
                        .open(&path)
                        .unwrap()
                        .set_len(1)
                        .unwrap();
                    Kind::Shrunk
                }
                "replace" => {
                    let other = path.with_extension("new");
                    std::fs::write(&other, b"replacement\n").unwrap();
                    std::fs::rename(other, &path).unwrap();
                    Kind::Replaced
                }
                _ => {
                    let index = if mode == "guard" { 8190 } else { 0 };
                    bytes[index] = b'b';
                    bytes.push(b'\n');
                    std::fs::write(&path, bytes).unwrap();
                    Kind::PrefixMismatch
                }
            };
            assert_eq!(records(&mut c, 8193), Err(expected), "{mode}");
            assert_eq!(records(&mut c, 8193), Err(expected));
        }
    }

    #[test]
    fn s0o_complete_large_records_keep_existing_partial_limit() {
        let mut bytes = vec![b'a'; MAX_PARTIAL_BYTES + 1];
        bytes.push(b'\n');
        let (_dir, mut c) = fixture(&bytes, 0);
        assert_eq!(
            records(&mut c, u64::MAX).unwrap()[0].line.len(),
            MAX_PARTIAL_BYTES + 1
        );
        let (_dir, mut c) = fixture(&bytes[..bytes.len() - 1], 0);
        assert_eq!(records(&mut c, u64::MAX), Err(Kind::Oversized));
    }

    #[cfg(unix)]
    #[test]
    fn s0o_observation_rejects_volume_renumber_without_prefix_proof() {
        let (_dir, mut c) = fixture(b"{}\n", 0);
        let path = c.source().path.clone();
        let source = c.source().clone();
        let _shift = renumber::shift(&path, 16);
        assert!(ObservationCapture::open(source, 0).is_err());
        assert_eq!(records(&mut c, 4), Err(Kind::Replaced));
    }

    #[cfg(unix)]
    #[test]
    fn s0o_races_reject_rotation_and_truncation_during_read() {
        for rotate in [false, true] {
            let (_dir, mut c) = fixture(b"{}\n", 0);
            let path = c.source().path.clone();
            io_trace::after_read_at(0, move || {
                std::thread::spawn(move || {
                    if rotate {
                        let other = path.with_extension("new");
                        std::fs::write(&other, b"{}\n").unwrap();
                        std::fs::rename(other, path).unwrap();
                    } else {
                        File::options()
                            .write(true)
                            .open(path)
                            .unwrap()
                            .set_len(1)
                            .unwrap();
                    }
                })
                .join()
                .unwrap();
            });
            assert_eq!(
                records(&mut c, 3),
                Err(if rotate { Kind::Replaced } else { Kind::Shrunk })
            );
        }
    }

    #[test]
    fn s0o_guard_revalidates_consumed_bytes_without_a_partial() {
        let (_dir, mut c) = fixture(b"alpha\n", 0);
        assert_eq!(records(&mut c, 6).unwrap().len(), 1);
        std::fs::write(&c.source().path, b"Alpha\n").unwrap();
        assert_eq!(records(&mut c, 0), Err(Kind::PrefixMismatch));
    }
}
