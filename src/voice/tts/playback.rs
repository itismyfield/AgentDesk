//! Chunked TTS playback with synthesis prefetch.

use super::{
    TtsRuntime, TtsSynthesisKind,
    chunks::{IncrementalTtsChunkQueue, split_for_tts},
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use songbird::{
    Event, EventContext, EventHandler, events::TrackEvent, input::File, tracks::TrackHandle,
};
use std::{
    io::ErrorKind,
    path::Path,
    path::PathBuf,
    sync::{Arc, Mutex as StdMutex},
    time::{Duration, Instant},
};
use tokio::{
    fs,
    sync::{Mutex, mpsc, oneshot},
};
use tokio_util::sync::CancellationToken;
use tracing::warn;

pub(crate) const DEFAULT_TTS_CHUNK_MAX_CHARS: usize = 220;

pub(crate) const DEFAULT_STREAMING_TTS_QUEUE_CAPACITY: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChunkedPlaybackReport {
    pub(crate) chunk_count: usize,
    pub(crate) played_chunks: usize,
    pub(crate) first_chunk_synthesis_ms: Option<u128>,
    pub(crate) first_audio_start_ms: Option<u128>,
}

#[derive(Debug)]
struct SynthesizedChunk {
    index: usize,
    path: PathBuf,
    synthesis_elapsed: Duration,
}

#[derive(Debug)]
pub(crate) struct StreamingTtsChunkSender {
    queue: IncrementalTtsChunkQueue,
    tx: mpsc::Sender<String>,
}

impl StreamingTtsChunkSender {
    pub(crate) async fn push_text(&mut self, text: &str) -> Result<()> {
        self.queue.push_text(text);
        self.flush_ready().await
    }

    pub(crate) async fn finish(mut self) -> Result<()> {
        self.queue.finish();
        self.flush_ready().await
    }

    async fn flush_ready(&mut self) -> Result<()> {
        while let Some(chunk) = self.queue.pop_ready() {
            self.tx
                .send(chunk)
                .await
                .map_err(|_| anyhow::anyhow!("streaming TTS playback receiver dropped"))?;
        }
        Ok(())
    }
}

pub(crate) fn streaming_tts_chunk_channel(
    max_chars: usize,
) -> (StreamingTtsChunkSender, mpsc::Receiver<String>) {
    streaming_tts_chunk_channel_with_capacity(max_chars, DEFAULT_STREAMING_TTS_QUEUE_CAPACITY)
}

pub(crate) fn streaming_tts_chunk_channel_with_capacity(
    max_chars: usize,
    capacity: usize,
) -> (StreamingTtsChunkSender, mpsc::Receiver<String>) {
    let (tx, rx) = mpsc::channel(capacity.max(1));
    (
        StreamingTtsChunkSender {
            queue: IncrementalTtsChunkQueue::new(max_chars),
            tx,
        },
        rx,
    )
}

pub(crate) async fn play_chunked_with_prefetch<F>(
    call_lock: Arc<Mutex<songbird::Call>>,
    tts: TtsRuntime,
    text: String,
    max_chars: usize,
    cancellation: CancellationToken,
    on_track_start: F,
) -> Result<ChunkedPlaybackReport>
where
    F: Fn(TrackHandle) + Send + Sync + 'static,
{
    let chunks = split_for_tts(&text, max_chars);
    if chunks.is_empty() {
        return Ok(ChunkedPlaybackReport {
            chunk_count: 0,
            played_chunks: 0,
            first_chunk_synthesis_ms: None,
            first_audio_start_ms: None,
        });
    }
    let total_chunks = chunks.len();

    play_prefetched_chunks(
        call_lock,
        tts,
        chunks,
        Some(total_chunks),
        cancellation,
        on_track_start,
    )
    .await
}

pub(crate) async fn play_streaming_chunks_with_prefetch<F>(
    call_lock: Arc<Mutex<songbird::Call>>,
    tts: TtsRuntime,
    chunks_rx: mpsc::Receiver<String>,
    cancellation: CancellationToken,
    on_track_start: F,
) -> Result<ChunkedPlaybackReport>
where
    F: Fn(TrackHandle) + Send + Sync + 'static,
{
    play_prefetched_chunk_receiver(
        call_lock,
        tts,
        chunks_rx,
        None,
        cancellation,
        on_track_start,
    )
    .await
}

async fn play_prefetched_chunks<F>(
    call_lock: Arc<Mutex<songbird::Call>>,
    tts: TtsRuntime,
    chunks: Vec<String>,
    total_chunks: Option<usize>,
    cancellation: CancellationToken,
    on_track_start: F,
) -> Result<ChunkedPlaybackReport>
where
    F: Fn(TrackHandle) + Send + Sync + 'static,
{
    let (chunks_tx, chunks_rx) = mpsc::channel(chunks.len().max(1));
    for chunk in chunks {
        chunks_tx
            .send(chunk)
            .await
            .map_err(|_| anyhow::anyhow!("chunked TTS playback receiver dropped"))?;
    }
    drop(chunks_tx);
    play_prefetched_chunk_receiver(
        call_lock,
        tts,
        chunks_rx,
        total_chunks,
        cancellation,
        on_track_start,
    )
    .await
}

async fn play_prefetched_chunk_receiver<F>(
    call_lock: Arc<Mutex<songbird::Call>>,
    tts: TtsRuntime,
    mut chunks_rx: mpsc::Receiver<String>,
    total_chunks: Option<usize>,
    cancellation: CancellationToken,
    on_track_start: F,
) -> Result<ChunkedPlaybackReport>
where
    F: Fn(TrackHandle) + Send + Sync + 'static,
{
    let playback_started_at = Instant::now();
    let (tx, mut rx) = mpsc::channel::<Result<SynthesizedChunk>>(2);
    let synth_cancellation = cancellation.clone();
    let synth_task = tokio::spawn(async move {
        let mut index = 0;
        while let Some(chunk) = chunks_rx.recv().await {
            if synth_cancellation.is_cancelled() {
                break;
            }

            let started_at = Instant::now();
            let output = tts
                .synthesize(&chunk, TtsSynthesisKind::Final)
                .await
                .with_context(|| {
                    format!(
                        "synthesize final TTS chunk {}{}",
                        index + 1,
                        total_chunks
                            .map(|total| format!("/{total}"))
                            .unwrap_or_default()
                    )
                })?;
            let synthesized = SynthesizedChunk {
                index,
                path: output.path,
                synthesis_elapsed: started_at.elapsed(),
            };
            if tx.send(Ok(synthesized)).await.is_err() {
                break;
            }
            index += 1;
        }
        Ok::<(), anyhow::Error>(())
    });

    let mut report = ChunkedPlaybackReport {
        chunk_count: total_chunks.unwrap_or(0),
        played_chunks: 0,
        first_chunk_synthesis_ms: None,
        first_audio_start_ms: None,
    };

    while let Some(synthesized) = rx.recv().await {
        // F16 (#2046): mpsc capacity=2 라 이미 합성 완료된 후속 chunk 가 채널/synth_task
        // 에 남아 있을 수 있다. ?-early-return 으로 unwind 되면 그 mp3 파일이 디스크에
        // 누수되므로 에러 path 에서 명시적으로 cleanup + synth_task.abort 한다.
        let synthesized = match synthesized {
            Ok(value) => value,
            Err(error) => {
                synth_task.abort();
                let _ = synth_task.await;
                cleanup_queued_synthesized_chunks(&mut rx).await;
                return Err(error);
            }
        };
        if synthesized.index == 0 {
            report.first_chunk_synthesis_ms = Some(synthesized.synthesis_elapsed.as_millis());
        }
        if cancellation.is_cancelled() {
            cleanup_synthesized_chunk(&synthesized.path).await;
            break;
        }

        report.chunk_count = report.chunk_count.max(synthesized.index + 1);
        let input = File::new(synthesized.path.clone()).into();
        let track = {
            let mut call = call_lock.lock().await;
            call.play_input(input)
        };
        on_track_start(track.clone());
        if report.first_audio_start_ms.is_none() {
            report.first_audio_start_ms = Some(playback_started_at.elapsed().as_millis());
        }

        tracing::info!(
            chunk = synthesized.index + 1,
            total_chunks = ?total_chunks,
            path = %synthesized.path.display(),
            synthesis_ms = synthesized.synthesis_elapsed.as_millis(),
            "voice final TTS chunk playback started"
        );

        tokio::select! {
            result = wait_for_track_end(track.clone()) => {
                let wait_result = result.with_context(|| {
                    format!(
                        "wait for final TTS chunk {}{} playback",
                        synthesized.index + 1,
                        total_chunks
                            .map(|total| format!("/{total}"))
                            .unwrap_or_default()
                    )
                });
                cleanup_synthesized_chunk(&synthesized.path).await;
                if let Err(error) = wait_result {
                    // F16 (#2046): wait_for_track_end 실패 시에도 후속 chunk 누수 방지.
                    synth_task.abort();
                    let _ = synth_task.await;
                    cleanup_queued_synthesized_chunks(&mut rx).await;
                    return Err(error);
                }
                report.played_chunks += 1;
            }
            _ = cancellation.cancelled() => {
                let _ = track.stop();
                cleanup_synthesized_chunk(&synthesized.path).await;
                break;
            }
        }
    }

    let synth_result = if cancellation.is_cancelled() {
        synth_task.abort();
        let _ = synth_task.await;
        Ok(())
    } else {
        synth_task
            .await
            .context("join final TTS synthesis prefetch task")?
    };
    cleanup_queued_synthesized_chunks(&mut rx).await;
    synth_result?;

    Ok(report)
}

async fn cleanup_queued_synthesized_chunks(rx: &mut mpsc::Receiver<Result<SynthesizedChunk>>) {
    while let Ok(Ok(synthesized)) = rx.try_recv() {
        cleanup_synthesized_chunk(&synthesized.path).await;
    }
}

async fn cleanup_synthesized_chunk(path: &Path) {
    match fs::remove_file(path).await {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => {
            warn!(
                path = %path.display(),
                %error,
                "failed to remove voice final TTS chunk after playback"
            );
        }
    }
}

async fn wait_for_track_end(track: TrackHandle) -> Result<()> {
    let (tx, rx) = oneshot::channel();
    track
        .add_event(
            Event::Track(TrackEvent::End),
            TrackEndNotifier {
                tx: StdMutex::new(Some(tx)),
            },
        )
        .map_err(|error| anyhow::anyhow!("attach TTS track end listener: {error}"))?;
    rx.await.context("TTS track end listener dropped")?;
    Ok(())
}

struct TrackEndNotifier {
    tx: StdMutex<Option<oneshot::Sender<()>>>,
}

#[async_trait]
impl EventHandler for TrackEndNotifier {
    async fn act(&self, _ctx: &EventContext<'_>) -> Option<Event> {
        if let Ok(mut tx) = self.tx.lock() {
            if let Some(tx) = tx.take() {
                let _ = tx.send(());
            }
        }
        Some(Event::Cancel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cleanup_queued_synthesized_chunks_removes_prefetched_files() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.mp3");
        let second = dir.path().join("second.mp3");
        fs::write(&first, b"first").await.unwrap();
        fs::write(&second, b"second").await.unwrap();

        let (tx, mut rx) = mpsc::channel(2);
        tx.send(Ok(SynthesizedChunk {
            index: 0,
            path: first.clone(),
            synthesis_elapsed: Duration::from_millis(5),
        }))
        .await
        .unwrap();
        tx.send(Ok(SynthesizedChunk {
            index: 1,
            path: second.clone(),
            synthesis_elapsed: Duration::from_millis(6),
        }))
        .await
        .unwrap();
        drop(tx);

        cleanup_queued_synthesized_chunks(&mut rx).await;

        assert!(!first.exists());
        assert!(!second.exists());
    }

    #[tokio::test]
    async fn streaming_tts_chunk_sender_flushes_sentence_boundaries() {
        let (mut tx, mut rx) = streaming_tts_chunk_channel_with_capacity(80, 2);

        tx.push_text("첫 문장입니다. 아직").await.unwrap();
        assert_eq!(rx.recv().await.as_deref(), Some("첫 문장입니다."));
        assert!(rx.try_recv().is_err());

        tx.push_text(" 끝나지 않음").await.unwrap();
        assert!(rx.try_recv().is_err());

        tx.finish().await.unwrap();
        assert_eq!(rx.recv().await.as_deref(), Some("아직 끝나지 않음"));
        assert!(rx.recv().await.is_none());
    }
}
