#![allow(dead_code)]

// === UTF-8 safe string slicing utilities ===
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

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

/// Format file size in human-readable format
pub fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes < KB {
        format!("{} B", bytes)
    } else if bytes < MB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else if bytes < GB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    }
}

/// Format file permissions in short format (rwxrwxrwx)
#[cfg(unix)]
pub fn format_permissions_short(mode: u32) -> String {
    const PERMS: [&str; 8] = ["---", "--x", "-w-", "-wx", "r--", "r-x", "rw-", "rwx"];

    let owner = PERMS[((mode >> 6) & 7) as usize];
    let group = PERMS[((mode >> 3) & 7) as usize];
    let other = PERMS[(mode & 7) as usize];

    format!("{}{}{}", owner, group, other)
}

#[cfg(not(unix))]
pub fn format_permissions_short(_mode: u32) -> String {
    String::new()
}

/// Format file permissions with type prefix
#[cfg(unix)]
pub fn format_permissions(mode: u32) -> String {
    const PERMS: [&str; 8] = ["---", "--x", "-w-", "-wx", "r--", "r-x", "rw-", "rwx"];

    let owner = PERMS[((mode >> 6) & 7) as usize];
    let group = PERMS[((mode >> 3) & 7) as usize];
    let other = PERMS[(mode & 7) as usize];

    let file_type = if (mode & 0o170000) == 0o040000 {
        'd'
    } else if (mode & 0o170000) == 0o120000 {
        'l'
    } else {
        '-'
    };

    format!(
        "{}{}{}{} ({:o})",
        file_type,
        owner,
        group,
        other,
        mode & 0o777
    )
}

#[cfg(not(unix))]
pub fn format_permissions(_mode: u32) -> String {
    String::new()
}

// === CJK-aware display width utilities ===

/// 표시 너비(display width) 기준으로 문자열을 잘라낸다.
/// 전각 문자가 경계에 걸리면 공백으로 패딩하여 정확히 max_width 칸을 채운다.
pub fn truncate_to_display_width(s: &str, max_width: usize) -> String {
    let mut width = 0;
    let mut result = String::new();
    for c in s.chars() {
        let cw = c.width().unwrap_or(1);
        if width + cw > max_width {
            break;
        }
        result.push(c);
        width += cw;
    }
    // 전각 문자 경계에서 잘린 경우 공백 패딩
    while width < max_width {
        result.push(' ');
        width += 1;
    }
    result
}

/// 표시 너비 기준으로 문자열을 target_width 칸에 맞춰 우측 공백 패딩한다.
/// 이미 target_width 이상이면 잘라낸다.
pub fn pad_to_display_width(s: &str, target_width: usize) -> String {
    let current = s.width();
    if current >= target_width {
        truncate_to_display_width(s, target_width)
    } else {
        format!("{}{}", s, " ".repeat(target_width - current))
    }
}

/// 표시 너비 기준으로 잘림 + "..." 접미사를 붙인다.
/// max_width 이하이면 원본 반환.
pub fn truncate_with_ellipsis(s: &str, max_width: usize) -> String {
    if s.width() <= max_width {
        return s.to_string();
    }
    if max_width <= 3 {
        return ".".repeat(max_width);
    }
    let truncated = truncate_to_display_width(s, max_width.saturating_sub(3));
    let trimmed = truncated.trim_end();
    format!("{}...", trimmed)
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

/// 표시 너비 기준으로 뒤에서부터 max_width 칸 이내의 접미사를 반환한다.
/// "..." 접두사 없이 순수 접미사만 반환. 호출자가 "..." 등을 붙인다.
pub fn display_width_suffix(s: &str, max_width: usize) -> String {
    if s.width() <= max_width {
        return s.to_string();
    }
    let chars: Vec<char> = s.chars().collect();
    let mut width = 0;
    let mut start_idx = chars.len();
    for i in (0..chars.len()).rev() {
        let cw = chars[i].width().unwrap_or(1);
        if width + cw > max_width {
            break;
        }
        width += cw;
        start_idx = i;
    }
    chars[start_idx..].iter().collect()
}
