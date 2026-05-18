//! TTS text chunking with Korean-friendly sentence boundaries.

use std::collections::VecDeque;

const DEFAULT_MAX_CHARS: usize = 220;

pub(crate) fn split_for_tts(text: &str, max_chars: usize) -> Vec<String> {
    let max_chars = if max_chars == 0 {
        DEFAULT_MAX_CHARS
    } else {
        max_chars
    };
    pack_sentence_segments(sentence_segments(text), max_chars)
}

#[derive(Debug, Clone)]
pub(crate) struct IncrementalTtsChunkQueue {
    max_chars: usize,
    pending_text: String,
    ready_chunks: VecDeque<String>,
}

impl IncrementalTtsChunkQueue {
    pub(crate) fn new(max_chars: usize) -> Self {
        let max_chars = if max_chars == 0 {
            DEFAULT_MAX_CHARS
        } else {
            max_chars
        };
        Self {
            max_chars,
            pending_text: String::new(),
            ready_chunks: VecDeque::new(),
        }
    }

    pub(crate) fn push_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.pending_text.push_str(text);
        self.flush_ready_segments(false);
    }

    pub(crate) fn finish(&mut self) {
        self.flush_ready_segments(true);
    }

    pub(crate) fn pop_ready(&mut self) -> Option<String> {
        self.ready_chunks.pop_front()
    }

    pub(crate) fn drain_ready(&mut self) -> Vec<String> {
        self.ready_chunks.drain(..).collect()
    }

    pub(crate) fn has_pending_text(&self) -> bool {
        !self.pending_text.trim().is_empty()
    }

    fn flush_ready_segments(&mut self, finishing: bool) {
        let segments = sentence_segments(&self.pending_text);
        if segments.is_empty() {
            self.pending_text.clear();
            return;
        }

        let ready_count = if finishing || ends_with_sentence_boundary(&self.pending_text) {
            segments.len()
        } else {
            segments.len().saturating_sub(1)
        };
        if ready_count == 0 {
            if let Some(last) = segments.last() {
                self.flush_oversized_tail_or_keep_pending(last);
            }
            return;
        }

        let ready_segments = segments[..ready_count].to_vec();
        self.ready_chunks
            .extend(pack_sentence_segments(ready_segments, self.max_chars));

        if ready_count < segments.len() {
            self.pending_text = segments[ready_count..].join(" ");
        } else {
            self.pending_text.clear();
        }
    }

    fn flush_oversized_tail_or_keep_pending(&mut self, tail: &str) {
        if char_len(tail) < self.max_chars {
            self.pending_text = tail.to_string();
            return;
        }

        let mut chunks = split_long_segment(tail, self.max_chars);
        match chunks.len() {
            0 => self.pending_text.clear(),
            1 => {
                self.ready_chunks.push_back(chunks.remove(0));
                self.pending_text.clear();
            }
            _ => {
                let pending = chunks.pop().unwrap_or_default();
                self.ready_chunks.extend(chunks);
                self.pending_text = pending;
            }
        }
    }
}

fn pack_sentence_segments(segments: Vec<String>, max_chars: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();

    for segment in segments {
        if char_len(&segment) > max_chars {
            flush_current(&mut chunks, &mut current);
            chunks.extend(split_long_segment(&segment, max_chars));
            continue;
        }

        let next_len = if current.is_empty() {
            char_len(&segment)
        } else {
            char_len(&current) + 1 + char_len(&segment)
        };
        if next_len > max_chars {
            flush_current(&mut chunks, &mut current);
        }

        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(&segment);
    }

    flush_current(&mut chunks, &mut current);
    chunks
}

fn sentence_segments(text: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut saw_boundary = false;

    for ch in text.chars() {
        if ch == '\n' || ch == '\r' {
            flush_current(&mut segments, &mut current);
            saw_boundary = false;
            continue;
        }

        if ch.is_whitespace() {
            if saw_boundary {
                flush_current(&mut segments, &mut current);
                saw_boundary = false;
            } else if !current.is_empty() && !current.ends_with(' ') {
                current.push(' ');
            }
            continue;
        }

        if saw_boundary && !is_closing_punctuation(ch) {
            flush_current(&mut segments, &mut current);
            saw_boundary = false;
        }

        current.push(ch);
        if is_sentence_boundary(ch) {
            saw_boundary = true;
        }
    }

    flush_current(&mut segments, &mut current);
    segments
}

fn split_long_segment(segment: &str, max_chars: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();

    for word in segment.split_whitespace() {
        if char_len(word) > max_chars {
            flush_current(&mut chunks, &mut current);
            chunks.extend(split_by_char_count(word, max_chars));
            continue;
        }

        let next_len = if current.is_empty() {
            char_len(word)
        } else {
            char_len(&current) + 1 + char_len(word)
        };
        if next_len > max_chars {
            flush_current(&mut chunks, &mut current);
        }

        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }

    flush_current(&mut chunks, &mut current);
    chunks
}

fn split_by_char_count(text: &str, max_chars: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if char_len(&current) >= max_chars {
            flush_current(&mut chunks, &mut current);
        }
        current.push(ch);
    }
    flush_current(&mut chunks, &mut current);
    chunks
}

fn flush_current(chunks: &mut Vec<String>, current: &mut String) {
    let trimmed = current.trim();
    if !trimmed.is_empty() {
        chunks.push(trimmed.to_string());
    }
    current.clear();
}

fn char_len(text: &str) -> usize {
    text.chars().count()
}

fn is_sentence_boundary(ch: char) -> bool {
    matches!(ch, '.' | '!' | '?' | '。' | '！' | '？' | '…')
}

fn is_closing_punctuation(ch: char) -> bool {
    matches!(
        ch,
        '"' | '\'' | ')' | ']' | '}' | '”' | '’' | '」' | '』' | '）' | '】'
    )
}

fn ends_with_sentence_boundary(text: &str) -> bool {
    for ch in text.chars().rev() {
        if ch == '\n' || ch == '\r' {
            return true;
        }
        if ch.is_whitespace() {
            continue;
        }
        if is_closing_punctuation(ch) {
            continue;
        }
        return is_sentence_boundary(ch);
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_korean_sentences_on_punctuation_boundaries() {
        let chunks = split_for_tts(
            "첫 번째 문장입니다. 두 번째 문장도 자연스럽게 이어집니다! 마지막 질문인가요?",
            28,
        );

        assert_eq!(
            chunks,
            vec![
                "첫 번째 문장입니다.",
                "두 번째 문장도 자연스럽게 이어집니다!",
                "마지막 질문인가요?"
            ]
        );
    }

    #[test]
    fn keeps_emoji_with_sentence_and_respects_char_limit() {
        let chunks = split_for_tts("좋아요 😊. 다음 단계로 갈게요. 완료했습니다.", 12);

        assert_eq!(
            chunks,
            vec!["좋아요 😊.", "다음 단계로 갈게요.", "완료했습니다."]
        );
        assert!(chunks.iter().all(|chunk| chunk.chars().count() <= 12));
    }

    #[test]
    fn splits_long_words_without_breaking_utf8() {
        let chunks = split_for_tts("가나다라마바사아자차카타파하", 5);

        assert_eq!(chunks, vec!["가나다라마", "바사아자차", "카타파하"]);
    }

    #[test]
    fn zero_max_uses_default_limit() {
        let text = "짧은 문장입니다.";

        assert_eq!(split_for_tts(text, 0), vec![text]);
    }

    #[test]
    fn splits_on_newline_boundaries() {
        let chunks = split_for_tts("첫 줄입니다\n둘째 줄입니다\n\n셋째 줄입니다", 12);

        assert_eq!(
            chunks,
            vec!["첫 줄입니다", "둘째 줄입니다", "셋째 줄입니다"]
        );
    }

    #[test]
    fn incremental_queue_emits_complete_sentences_and_keeps_tail_pending() {
        let mut queue = IncrementalTtsChunkQueue::new(80);

        queue.push_text("첫 문장입니다. 아직 두");
        assert_eq!(queue.drain_ready(), vec!["첫 문장입니다."]);
        assert!(queue.has_pending_text());

        queue.push_text(" 번째 문장");
        assert!(queue.drain_ready().is_empty());

        queue.push_text("입니다!");
        assert_eq!(queue.drain_ready(), vec!["아직 두 번째 문장입니다!"]);
        assert!(!queue.has_pending_text());
    }

    #[test]
    fn incremental_queue_finish_flushes_incomplete_tail() {
        let mut queue = IncrementalTtsChunkQueue::new(12);

        queue.push_text("좋아요. 마무리 중");
        assert_eq!(queue.drain_ready(), vec!["좋아요."]);
        queue.finish();

        assert_eq!(queue.drain_ready(), vec!["마무리 중"]);
        assert!(!queue.has_pending_text());
    }

    #[test]
    fn incremental_queue_treats_newline_as_ready_boundary() {
        let mut queue = IncrementalTtsChunkQueue::new(80);

        queue.push_text("첫 줄입니다\n둘째 줄");
        assert_eq!(queue.drain_ready(), vec!["첫 줄입니다"]);
        queue.push_text("입니다\n");
        assert_eq!(queue.drain_ready(), vec!["둘째 줄입니다"]);
        assert!(!queue.has_pending_text());
    }

    #[test]
    fn incremental_queue_flushes_long_unpunctuated_text() {
        let mut queue = IncrementalTtsChunkQueue::new(10);

        queue.push_text("하나 둘 셋 넷 다섯 여섯");

        assert_eq!(queue.drain_ready(), vec!["하나 둘 셋 넷"]);
        assert!(queue.has_pending_text());
        queue.finish();
        assert_eq!(queue.drain_ready(), vec!["다섯 여섯"]);
    }

    #[test]
    fn incremental_queue_flushes_single_oversized_word() {
        let mut queue = IncrementalTtsChunkQueue::new(5);

        queue.push_text("가나다라마바사아자");

        assert_eq!(queue.drain_ready(), vec!["가나다라마"]);
        assert!(queue.has_pending_text());
    }
}
