use anyhow::{Result, bail};

pub(crate) const WHISPER_STREAM_SAMPLE_RATE_HZ: u32 = 16_000;
pub(crate) const DEFAULT_STREAM_STEP_MS: u32 = 500;
pub(crate) const DEFAULT_STREAM_LENGTH_MS: u32 = 5_000;
pub(crate) const DEFAULT_STREAM_KEEP_MS: u32 = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StreamingOverlapConfig {
    pub(crate) sample_rate_hz: u32,
    pub(crate) step_ms: u32,
    pub(crate) length_ms: u32,
    pub(crate) keep_ms: u32,
}

impl Default for StreamingOverlapConfig {
    fn default() -> Self {
        Self {
            sample_rate_hz: WHISPER_STREAM_SAMPLE_RATE_HZ,
            step_ms: DEFAULT_STREAM_STEP_MS,
            length_ms: DEFAULT_STREAM_LENGTH_MS,
            keep_ms: DEFAULT_STREAM_KEEP_MS,
        }
    }
}

impl StreamingOverlapConfig {
    pub(crate) fn normalized(mut self) -> Self {
        self.length_ms = self.length_ms.max(self.step_ms);
        self.keep_ms = self.keep_ms.min(self.step_ms);
        self
    }

    pub(crate) fn validate(self) -> Result<Self> {
        let normalized = self.normalized();
        if normalized.sample_rate_hz == 0 {
            bail!("voice STT stream sample_rate_hz must be greater than zero");
        }
        if normalized.step_ms == 0 {
            bail!("voice STT stream step_ms must be greater than zero");
        }
        if normalized.length_ms == 0 {
            bail!("voice STT stream length_ms must be greater than zero");
        }
        Ok(normalized)
    }

    fn samples_for_ms(&self, ms: u32) -> usize {
        let samples = (u64::from(self.sample_rate_hz) * u64::from(ms)).div_ceil(1_000);
        samples.min(usize::MAX as u64) as usize
    }

    pub(crate) fn step_samples(&self) -> usize {
        self.samples_for_ms(self.step_ms)
    }

    pub(crate) fn length_samples(&self) -> usize {
        self.samples_for_ms(self.length_ms)
    }

    pub(crate) fn keep_samples(&self) -> usize {
        self.samples_for_ms(self.keep_ms)
    }

    fn context_reset_interval(&self) -> u32 {
        (self.length_ms / self.step_ms).saturating_sub(1).max(1)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StreamingDecodeWindowMeta {
    pub(crate) sequence: u64,
    pub(crate) sample_rate_hz: u32,
    pub(crate) window_start_sample: u64,
    pub(crate) window_end_sample: u64,
    pub(crate) new_audio_start_sample: u64,
    pub(crate) is_final: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct StreamingDecodeWindow {
    pub(crate) meta: StreamingDecodeWindowMeta,
    pub(crate) samples: Vec<f32>,
}

#[derive(Debug, Clone)]
pub(crate) struct WhisperStreamOverlapSegmenter {
    config: StreamingOverlapConfig,
    pending_new: Vec<f32>,
    previous_window: Vec<f32>,
    next_sequence: u64,
    emitted_new_samples: u64,
    windows_since_context_reset: u32,
}

impl WhisperStreamOverlapSegmenter {
    pub(crate) fn new(config: StreamingOverlapConfig) -> Result<Self> {
        let config = config.validate()?;
        Ok(Self {
            config,
            pending_new: Vec::new(),
            previous_window: Vec::new(),
            next_sequence: 0,
            emitted_new_samples: 0,
            windows_since_context_reset: 0,
        })
    }

    // reason: voice runtime is wired only when voice config is enabled; no
    // compile target exercises it. See #3034.
    #[allow(dead_code)]
    pub(crate) fn config(&self) -> StreamingOverlapConfig {
        self.config
    }

    pub(crate) fn feed(&mut self, samples: &[f32]) -> Vec<StreamingDecodeWindow> {
        self.pending_new.extend_from_slice(samples);
        let mut windows = Vec::new();
        let step_samples = self.config.step_samples();
        while self.pending_new.len() >= step_samples {
            let new = self.pending_new.drain(..step_samples).collect::<Vec<_>>();
            windows.push(self.build_window(new, false));
        }
        windows
    }

    pub(crate) fn finish(&mut self) -> Option<StreamingDecodeWindow> {
        if self.pending_new.is_empty() {
            return None;
        }
        let new = self.pending_new.drain(..).collect::<Vec<_>>();
        Some(self.build_window(new, true))
    }

    fn build_window(&mut self, new: Vec<f32>, is_final: bool) -> StreamingDecodeWindow {
        let new_start = self.emitted_new_samples;
        let keep_plus_length = self
            .config
            .keep_samples()
            .saturating_add(self.config.length_samples());
        let samples_to_take = self
            .previous_window
            .len()
            .min(keep_plus_length.saturating_sub(new.len()));
        let previous_start = self.previous_window.len().saturating_sub(samples_to_take);

        let mut samples = Vec::with_capacity(samples_to_take + new.len());
        samples.extend_from_slice(&self.previous_window[previous_start..]);
        samples.extend_from_slice(&new);

        let window_start_sample = new_start.saturating_sub(samples_to_take as u64);
        self.emitted_new_samples = self.emitted_new_samples.saturating_add(new.len() as u64);
        let window_end_sample = self.emitted_new_samples;
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.saturating_add(1);
        self.previous_window = samples.clone();
        self.windows_since_context_reset = self.windows_since_context_reset.saturating_add(1);
        if self.windows_since_context_reset >= self.config.context_reset_interval() {
            self.trim_previous_window_to_keep_samples();
            self.windows_since_context_reset = 0;
        }

        StreamingDecodeWindow {
            meta: StreamingDecodeWindowMeta {
                sequence,
                sample_rate_hz: self.config.sample_rate_hz,
                window_start_sample,
                window_end_sample,
                new_audio_start_sample: new_start,
                is_final,
            },
            samples,
        }
    }

    fn trim_previous_window_to_keep_samples(&mut self) {
        let keep_samples = self.config.keep_samples();
        if self.previous_window.len() <= keep_samples {
            return;
        }
        let start = self.previous_window.len().saturating_sub(keep_samples);
        self.previous_window.drain(..start);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pcm(start: usize, end: usize) -> Vec<f32> {
        (start..end).map(|sample| sample as f32).collect()
    }

    #[test]
    fn streaming_overlap_config_normalizes_to_whisper_stream_bounds() {
        let config = StreamingOverlapConfig {
            sample_rate_hz: 1_000,
            step_ms: 20,
            length_ms: 10,
            keep_ms: 30,
        }
        .validate()
        .unwrap();

        assert_eq!(config.step_ms, 20);
        assert_eq!(config.length_ms, 20);
        assert_eq!(config.keep_ms, 20);
        assert_eq!(config.step_samples(), 20);
        assert_eq!(config.length_samples(), 20);
        assert_eq!(config.keep_samples(), 20);
        assert_eq!(config.context_reset_interval(), 1);
    }

    /// #3914: range validation must reject the unrecoverable cases (zero step /
    /// zero sample rate) and normalize the recoverable inversion
    /// (`length_ms < keep_ms`) so the segmenter can never run with `keep > step`
    /// or `length < step`.
    #[test]
    fn streaming_overlap_config_validation_guards_out_of_range_windows() {
        assert!(
            StreamingOverlapConfig {
                sample_rate_hz: 16_000,
                step_ms: 0,
                length_ms: 100,
                keep_ms: 10,
            }
            .validate()
            .is_err(),
            "step_ms = 0 must be rejected (would otherwise spin the feed loop)"
        );
        assert!(
            StreamingOverlapConfig {
                sample_rate_hz: 0,
                step_ms: 10,
                length_ms: 100,
                keep_ms: 10,
            }
            .validate()
            .is_err(),
            "sample_rate_hz = 0 must be rejected"
        );

        // length_ms < keep_ms is normalized, never inverted: keep <= step <= length.
        let normalized = StreamingOverlapConfig {
            sample_rate_hz: 16_000,
            step_ms: 50,
            length_ms: 10,
            keep_ms: 999,
        }
        .validate()
        .unwrap();
        assert!(normalized.keep_ms <= normalized.step_ms);
        assert!(normalized.step_ms <= normalized.length_ms);
    }

    #[test]
    fn streaming_overlap_segmenter_emits_step_windows_with_keep_resets() {
        let mut segmenter = WhisperStreamOverlapSegmenter::new(StreamingOverlapConfig {
            sample_rate_hz: 1_000,
            step_ms: 4,
            length_ms: 8,
            keep_ms: 2,
        })
        .unwrap();

        assert!(segmenter.feed(&pcm(0, 3)).is_empty());
        let windows = segmenter.feed(&pcm(3, 8));

        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].samples, pcm(0, 4));
        assert_eq!(windows[0].meta.sequence, 0);
        assert_eq!(windows[0].meta.window_start_sample, 0);
        assert_eq!(windows[0].meta.window_end_sample, 4);
        assert_eq!(windows[0].meta.new_audio_start_sample, 0);
        assert!(!windows[0].meta.is_final);

        assert_eq!(windows[1].samples, pcm(2, 8));
        assert_eq!(windows[1].meta.sequence, 1);
        assert_eq!(windows[1].meta.window_start_sample, 2);
        assert_eq!(windows[1].meta.window_end_sample, 8);
        assert_eq!(windows[1].meta.new_audio_start_sample, 4);
        assert!(!windows[1].meta.is_final);

        let windows = segmenter.feed(&pcm(8, 12));

        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].samples, pcm(6, 12));
        assert_eq!(windows[0].meta.sequence, 2);
        assert_eq!(windows[0].meta.window_start_sample, 6);
        assert_eq!(windows[0].meta.window_end_sample, 12);
        assert_eq!(windows[0].meta.new_audio_start_sample, 8);
    }

    #[test]
    fn streaming_overlap_segmenter_keeps_full_context_until_reset_interval() {
        let mut segmenter = WhisperStreamOverlapSegmenter::new(StreamingOverlapConfig {
            sample_rate_hz: 1_000,
            step_ms: 2,
            length_ms: 6,
            keep_ms: 1,
        })
        .unwrap();

        let first = segmenter.feed(&pcm(0, 2));
        assert_eq!(first[0].samples, pcm(0, 2));
        let second = segmenter.feed(&pcm(2, 4));
        assert_eq!(second[0].samples, pcm(0, 4));
        let third = segmenter.feed(&pcm(4, 6));
        assert_eq!(third[0].samples, pcm(3, 6));
    }

    #[test]
    fn streaming_overlap_segmenter_finishes_pending_tail_as_final_window() {
        let mut segmenter = WhisperStreamOverlapSegmenter::new(StreamingOverlapConfig {
            sample_rate_hz: 1_000,
            step_ms: 4,
            length_ms: 8,
            keep_ms: 2,
        })
        .unwrap();

        let windows = segmenter.feed(&pcm(0, 5));
        assert_eq!(windows.len(), 1);

        let final_window = segmenter.finish().unwrap();

        assert_eq!(final_window.samples, pcm(2, 5));
        assert_eq!(final_window.meta.sequence, 1);
        assert_eq!(final_window.meta.window_start_sample, 2);
        assert_eq!(final_window.meta.window_end_sample, 5);
        assert_eq!(final_window.meta.new_audio_start_sample, 4);
        assert!(final_window.meta.is_final);
        assert!(segmenter.finish().is_none());
    }
}
