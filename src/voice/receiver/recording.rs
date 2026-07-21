use super::*;

#[derive(Default)]
pub(crate) struct UserAudioState {
    pub(super) active: Option<ActiveUtterance>,
    pub(super) pending_io: bool,
    pub(super) segment_timer: Option<JoinHandle<()>>,
    pub(super) utterance_timer: Option<JoinHandle<()>>,
}

pub(crate) struct ActiveUtterance {
    pub(super) user_id: u64,
    pub(super) control_channel_id: Option<u64>,
    pub(super) utterance_id: String,
    pub(super) utterance_path: PathBuf,
    pub(super) utterance_writer: WavFileWriter,
    pub(super) segment_dir: PathBuf,
    pub(super) current_segment_path: Option<PathBuf>,
    pub(super) segment_writer: Option<WavFileWriter>,
    pub(super) segment_paths: Vec<PathBuf>,
    pub(super) next_segment_index: u32,
    pub(super) samples_written: usize,
    pub(super) started_at: String,
}

impl ActiveUtterance {
    pub(super) fn ensure_segment_writer(&mut self) -> Result<(), VoiceReceiverError> {
        if self.segment_writer.is_some() {
            return Ok(());
        }

        let segment_path = self.segment_dir.join(format!(
            "{}_segment_{:03}.wav",
            self.utterance_id, self.next_segment_index
        ));
        self.next_segment_index += 1;
        let segment_writer = create_wav_writer(&segment_path)?;
        self.current_segment_path = Some(segment_path);
        self.segment_writer = Some(segment_writer);
        Ok(())
    }

    pub(super) fn write_samples(&mut self, samples: &[i16]) -> Result<(), VoiceReceiverError> {
        for sample in samples {
            self.utterance_writer
                .write_sample(*sample)
                .map_err(|source| VoiceReceiverError::Wav {
                    path: self.utterance_path.clone(),
                    source,
                })?;
            if let Some(writer) = self.segment_writer.as_mut() {
                writer
                    .write_sample(*sample)
                    .map_err(|source| VoiceReceiverError::Wav {
                        path: self
                            .current_segment_path
                            .clone()
                            .unwrap_or_else(|| self.segment_dir.clone()),
                        source,
                    })?;
            }
        }
        self.samples_written += samples.len();
        Ok(())
    }

    pub(super) fn finish_segment(&mut self) -> Result<(), VoiceReceiverError> {
        let Some(writer) = self.segment_writer.take() else {
            return Ok(());
        };
        let Some(path) = self.current_segment_path.take() else {
            return Ok(());
        };
        writer
            .finalize()
            .map_err(|source| VoiceReceiverError::Wav {
                path: path.clone(),
                source,
            })?;
        self.segment_paths.push(path);
        Ok(())
    }

    pub(super) fn finalize(mut self) -> Result<CompletedUtterance, VoiceReceiverError> {
        self.finish_segment()?;
        self.utterance_writer
            .finalize()
            .map_err(|source| VoiceReceiverError::Wav {
                path: self.utterance_path.clone(),
                source,
            })?;
        Ok(CompletedUtterance {
            user_id: self.user_id,
            control_channel_id: self.control_channel_id,
            utterance_id: self.utterance_id,
            path: self.utterance_path,
            segment_paths: self.segment_paths,
            samples_written: self.samples_written,
            started_at: self.started_at,
            completed_at: chrono::Local::now().to_rfc3339(),
        })
    }
}

pub(crate) fn abort_timer(timer: Option<JoinHandle<()>>) {
    if let Some(timer) = timer {
        timer.abort();
    }
}

/// #2156: voice 시작 시 호출된다. 정확히 다음 2단계 레이아웃만 정리한다:
///   `<recordings_dir>/utterances/user_<id>/<utterance-id>.wav`
///   `<recordings_dir>/segments/user_<id>/<utterance-id>_segment_NNN.wav`
/// 더 깊거나 다른 레이아웃의 파일은 손대지 않는다 (운영자가 의도적으로 모아둔
/// 것일 수 있음). 디렉토리 자체는 남겨 매 utterance 가 다시 `create_dir_all`
/// 비용을 치르지 않게 한다. 에러는 debug 로그로만 흘려 GC 가 시작 흐름을
/// 막지 않게 한다.
///
/// 안전 가드:
/// - symlink user 디렉토리는 따라가지 않고 skip 한다 (외부 트리로 빠져 외부
///   파일을 지울 위험 차단).
/// - symlink 파일 entry 도 skip (자체 wav 가 아니라 외부 wav 를 가리킬 수 있음).
pub(crate) fn gc_voice_recordings_dir(recordings_dir: &Path) {
    let utterance_root = recordings_dir.join("utterances");
    let segment_root = recordings_dir.join("segments");
    let removed_utterances = gc_wav_subtree(&utterance_root);
    let removed_segments = gc_wav_subtree(&segment_root);
    if removed_utterances + removed_segments > 0 {
        tracing::info!(
            removed_utterances,
            removed_segments,
            recordings_dir = %recordings_dir.display(),
            "voice recordings GC removed accumulated wav files (#2156)"
        );
    }
}

pub(crate) fn gc_wav_subtree(root: &Path) -> usize {
    let mut removed = 0usize;
    // Root 자체(예: `utterances`, `segments`) 가 symlink 면 따라가지 않는다.
    // `fs::symlink_metadata` 는 마지막 컴포넌트에 대해 symlink 를 그대로 보고하므로
    // 외부 트리로 빠지는 GC 진입을 차단할 수 있다.
    match fs::symlink_metadata(root) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            tracing::debug!(
                path = %root.display(),
                "voice recordings GC skipped: root is a symlink"
            );
            return 0;
        }
        Ok(_) => {}
        Err(_) => return 0,
    }
    let Ok(top) = fs::read_dir(root) else {
        return 0;
    };
    for user_dir in top.flatten() {
        // Symlink user 디렉토리는 정책상 따라가지 않는다 — 외부 트리로 빠질 수 있음.
        // `DirEntry::file_type` 와 `symlink_metadata` 모두 마지막 컴포넌트의
        // symlink 를 그대로 보고하므로 어떤 쪽을 써도 무방하지만, root 검사와
        // 동일하게 `symlink_metadata` 로 통일해 보안 의도를 일관되게 표현한다.
        let user_path = user_dir.path();
        match fs::symlink_metadata(&user_path) {
            Ok(md) if md.file_type().is_symlink() => continue,
            Ok(_) => {}
            Err(_) => continue,
        }
        let Ok(entries) = fs::read_dir(&user_path) else {
            continue;
        };
        for entry in entries.flatten() {
            let entry_path = entry.path();
            // Symlink 파일도 skip — 가리키는 대상이 외부 wav 일 수 있다.
            match fs::symlink_metadata(&entry_path) {
                Ok(md) if md.file_type().is_symlink() => continue,
                Ok(_) => {}
                Err(_) => continue,
            }
            if entry_path.extension().and_then(|ext| ext.to_str()) != Some("wav") {
                continue;
            }
            match fs::remove_file(&entry_path) {
                Ok(()) => removed += 1,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => tracing::debug!(
                    error = %error,
                    path = %entry_path.display(),
                    "voice recordings GC could not remove file"
                ),
            }
        }
    }
    removed
}

pub(crate) fn create_dir_all(path: &Path) -> Result<(), VoiceReceiverError> {
    fs::create_dir_all(path).map_err(|source| VoiceReceiverError::CreateDir {
        path: path.to_path_buf(),
        source,
    })
}

pub(crate) fn create_wav_writer(path: &Path) -> Result<WavFileWriter, VoiceReceiverError> {
    WavWriter::create(path, wav_spec()).map_err(|source| VoiceReceiverError::Wav {
        path: path.to_path_buf(),
        source,
    })
}

pub(crate) fn wav_spec() -> WavSpec {
    WavSpec {
        channels: WAV_CHANNELS,
        sample_rate: WAV_SAMPLE_RATE,
        bits_per_sample: WAV_BITS_PER_SAMPLE,
        sample_format: SampleFormat::Int,
    }
}
