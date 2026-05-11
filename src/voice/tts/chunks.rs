//! TTS text chunking with Korean-friendly sentence boundaries.

const DEFAULT_MAX_CHARS: usize = 220;

pub(crate) fn split_for_tts(text: &str, max_chars: usize) -> Vec<String> {
    let max_chars = if max_chars == 0 {
        DEFAULT_MAX_CHARS
    } else {
        max_chars
    };
    let mut chunks = Vec::new();
    let mut current = String::new();

    for segment in sentence_segments(text) {
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
}
