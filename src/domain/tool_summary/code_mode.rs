use super::*;

#[derive(PartialEq)]
enum Token {
    Word(String),
    String(String),
    Mark(char),
}

// Only literal tool calls are inspected. No JavaScript is evaluated or interpolated.
pub(super) fn summary(source: &str, cwd: Option<&str>) -> Option<String> {
    let tokens = tokens(source)?;
    let mut calls = Vec::new();
    for at in 0..tokens.len().saturating_sub(4) {
        let [
            Token::Word(receiver),
            Token::Mark('.'),
            Token::Word(name),
            Token::Mark('('),
        ] = &tokens[at..at + 4]
        else {
            continue;
        };
        if receiver != "tools" || at > 0 && tokens[at - 1] == Token::Mark('.') {
            continue;
        }
        let Some((input, end)) = object(&tokens, at + 4) else {
            continue;
        };
        if tokens.get(end) != Some(&Token::Mark(')')) || tool_key(name) == "exec" {
            continue;
        }
        let label = class(name).replace('_', "\\_");
        let detail = safe_tool_summary(name, &input.to_string(), cwd);
        calls.push((label, detail));
    }
    let (label, detail) = calls.first()?;
    let mut out = match detail {
        Some(detail) => format!("{label} · {detail}"),
        None => label.clone(),
    };
    for (label, _) in calls.iter().skip(1).take(2) {
        out.push_str(&format!(" · {label}"));
    }
    if calls.len() > 3 {
        out.push_str(&format!(" · +{}", calls.len() - 3));
    }
    Some(out)
}

fn tokens(source: &str) -> Option<Vec<Token>> {
    let mut chars = source.chars().peekable();
    let mut out = Vec::new();
    while let Some(ch) = chars.next() {
        match ch {
            ch if ch.is_whitespace() => {}
            '/' => match chars.next()? {
                '/' => {
                    for ch in chars.by_ref() {
                        if ch == '\n' {
                            break;
                        }
                    }
                }
                '*' => {
                    let mut closed = false;
                    while let Some(ch) = chars.next() {
                        if ch == '*' && chars.peek() == Some(&'/') {
                            chars.next();
                            closed = true;
                            break;
                        }
                    }
                    if !closed {
                        return None;
                    }
                }
                _ => return None,
            },
            '\'' | '"' | '`' => {
                let mut value = String::new();
                let mut closed = false;
                while let Some(next) = chars.next() {
                    if next == ch {
                        closed = true;
                        break;
                    }
                    if ch == '`' && next == '$' && chars.peek() == Some(&'{') {
                        return None;
                    }
                    value.push(if next == '\\' {
                        match chars.next()? {
                            'n' => '\n',
                            'r' => '\r',
                            't' => '\t',
                            escaped @ ('\\' | '\'' | '"' | '`' | '/') => escaped,
                            _ => return None,
                        }
                    } else {
                        next
                    });
                }
                if !closed {
                    return None;
                }
                out.push(Token::String(value));
            }
            ch if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '$') => {
                let mut word = ch.to_string();
                while chars
                    .peek()
                    .is_some_and(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '$'))
                {
                    word.push(chars.next()?);
                }
                out.push(Token::Word(word));
            }
            ch => out.push(Token::Mark(ch)),
        }
    }
    Some(out)
}

fn object(tokens: &[Token], mut at: usize) -> Option<(Value, usize)> {
    if tokens.get(at) != Some(&Token::Mark('{')) {
        return None;
    }
    at += 1;
    let mut map = serde_json::Map::new();
    while tokens.get(at) != Some(&Token::Mark('}')) {
        let key = match tokens.get(at)? {
            Token::Word(key) | Token::String(key) => key,
            _ => return None,
        };
        if tokens.get(at + 1) != Some(&Token::Mark(':')) {
            return None;
        }
        let value = match tokens.get(at + 2)? {
            Token::String(value) => Value::String(value.clone()),
            Token::Word(value) => serde_json::from_str(value).ok()?,
            _ => return None,
        };
        if map.insert(key.clone(), value).is_some() {
            return None;
        }
        at += 3;
        match tokens.get(at)? {
            Token::Mark(',') => at += 1,
            Token::Mark('}') => {}
            _ => return None,
        }
    }
    Some((Value::Object(map), at + 1))
}
