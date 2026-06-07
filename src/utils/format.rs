// === UTF-8 safe string slicing utilities ===

/// Byte index를 가장 가까운 char boundary로 내림
pub fn floor_char_boundary(s: &str, index: usize) -> usize {
    if index >= s.len() {
        return s.len();
    }
    let mut i = index;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Byte index를 가장 가까운 char boundary로 올림
fn ceil_char_boundary(s: &str, index: usize) -> usize {
    if index >= s.len() {
        return s.len();
    }
    let mut i = index;
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// 문자열 뒤에서 max_bytes 이내의 char boundary에서 자름 (앞부분 생략용)
pub fn safe_suffix(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let start = s.len() - max_bytes;
    let boundary = ceil_char_boundary(s, start);
    &s[boundary..]
}

/// 문자열 앞에서 max_bytes 이내의 char boundary에서 자름 (뒷부분 생략용)
pub fn safe_prefix(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let boundary = floor_char_boundary(s, max_bytes);
    &s[..boundary]
}

/// String::truncate의 안전한 버전
pub fn safe_truncate(s: &mut String, max_bytes: usize) {
    if s.len() > max_bytes {
        let boundary = floor_char_boundary(s, max_bytes);
        s.truncate(boundary);
    }
}

/// 문자열 뒤에서 max_chars 글자 이내로 자르고, 앞에 "…"을 붙인다.
/// Discord 메시지 상태 표시용.
pub fn tail_with_ellipsis(text: &str, max_chars: usize) -> String {
    let char_count = text.chars().count();
    if char_count <= max_chars {
        return text.to_string();
    }

    if max_chars <= 1 {
        return "…".to_string();
    }

    let keep = max_chars.saturating_sub(1);
    let skip = char_count - keep;
    let byte_start = text.char_indices().nth(skip).map(|(i, _)| i).unwrap_or(0);
    format!("…{}", &text[byte_start..])
}

/// 문자열 뒤에서 max_bytes 바이트 이내로 자르고, 앞에 "…"을 붙인다.
/// 최종 문자열의 UTF-8 길이가 max_bytes를 넘지 않도록 보장한다.
pub fn tail_with_ellipsis_bytes(text: &str, max_bytes: usize) -> String {
    const ELLIPSIS: &str = "…";

    if text.len() <= max_bytes {
        return text.to_string();
    }

    if max_bytes == 0 {
        return String::new();
    }

    if max_bytes < ELLIPSIS.len() {
        return safe_suffix(text, max_bytes).to_string();
    }

    if max_bytes == ELLIPSIS.len() {
        return ELLIPSIS.to_string();
    }

    let suffix = safe_suffix(text, max_bytes.saturating_sub(ELLIPSIS.len()));
    format!("{ELLIPSIS}{suffix}")
}

/// `~` 또는 `~/...` 경로를 홈 디렉토리로 확장한다.
/// `~user/...` 형태는 확장하지 않고 그대로 반환한다.
pub fn expand_tilde_path(path: &str) -> std::path::PathBuf {
    if path == "~" || path.starts_with("~/") {
        if let Some(expanded) = crate::runtime_layout::expand_user_path(path) {
            return expanded;
        }
    }
    std::path::PathBuf::from(path)
}
