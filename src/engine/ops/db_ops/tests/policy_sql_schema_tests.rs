//! Static extraction of policy SQL for the Postgres schema check (#7351).
//!
//! Walks `policies/**/*.js` (minus `__tests__`), finds every
//! `agentdesk.db.query/execute/queryJson(...)` call and expands its first
//! argument into concrete SQL variants. This is a small JS subset evaluator:
//! string literals, templates, `+`, ternaries, `[...].join`,
//! `xs.map(() => "?").join`, local variables (nearest declaration before the
//! use plus later `=`/`+=`), global literal constants, and helper functions
//! whose top-level `return`s expand. Anything else becomes a `/*DYN:name*/`
//! marker; the PG test in `db_ops::tests` prepares only the variants without one.

use std::collections::HashMap;
use std::path::Path;

pub(super) const DYN_MARKER: &str = "/*DYN:";
const MAX_VARIANTS: usize = 16;
const MAX_DEPTH: usize = 32;

#[derive(Debug)]
pub(super) struct PolicySqlCall {
    pub(super) file: String,
    pub(super) line: usize,
    pub(super) method: String,
    pub(super) variants: Vec<String>,
    /// Set when the call (or a raw `agentdesk.db` access) could not be parsed.
    pub(super) unsupported: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Ident(String),
    Str(String),
    Num(String),
    Template(Vec<TplTok>),
    Punct(String),
    Regex,
}

#[derive(Debug, Clone, PartialEq)]
enum TplTok {
    Text(String),
    Expr(Vec<Token>),
}

#[derive(Debug, Clone, PartialEq)]
struct Token {
    tok: Tok,
    line: usize,
}

const PUNCTS: &[&str] = &[
    ">>>=", "...", "===", "!==", "**=", "<<=", ">>=", ">>>", "&&=", "||=", "??=", "=>", "==", "!=",
    "<=", ">=", "&&", "||", "??", "?.", "++", "--", "+=", "-=", "*=", "/=", "%=", "&=", "|=", "^=",
    "**", "<<", ">>",
];
const REGEX_KEYWORDS: &[&str] = &[
    "return",
    "typeof",
    "case",
    "do",
    "else",
    "in",
    "of",
    "new",
    "delete",
    "void",
    "throw",
    "instanceof",
    "yield",
    "await",
];

struct Lexer<'a> {
    src: &'a [u8],
    pos: usize,
    line: usize,
}

fn lex(text: &str) -> Result<Vec<Token>, String> {
    let mut lexer = Lexer {
        src: text.as_bytes(),
        pos: 0,
        line: 1,
    };
    lexer.lex_tokens(false)
}

fn is_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$'
}

impl Lexer<'_> {
    fn peek(&self, ahead: usize) -> Option<u8> {
        self.src.get(self.pos + ahead).copied()
    }

    /// Lexes until EOF, or until the `}` closing a template `${` when `in_template`.
    fn lex_tokens(&mut self, in_template: bool) -> Result<Vec<Token>, String> {
        let mut out: Vec<Token> = Vec::new();
        let mut depth = 0usize;
        while let Some(byte) = self.peek(0) {
            let line = self.line;
            match byte {
                b'\n' => {
                    self.line += 1;
                    self.pos += 1;
                }
                b' ' | b'\t' | b'\r' => self.pos += 1,
                b'/' if self.peek(1) == Some(b'/') => {
                    while self.peek(0).is_some_and(|b| b != b'\n') {
                        self.pos += 1;
                    }
                }
                b'/' if self.peek(1) == Some(b'*') => {
                    self.pos += 2;
                    while self.pos < self.src.len() && !self.src[self.pos..].starts_with(b"*/") {
                        if self.src[self.pos] == b'\n' {
                            self.line += 1;
                        }
                        self.pos += 1;
                    }
                    self.pos += 2;
                }
                b'\'' | b'"' => {
                    let value = self.lex_string(byte)?;
                    out.push(Token {
                        tok: Tok::Str(value),
                        line,
                    });
                }
                b'`' => {
                    let parts = self.lex_template()?;
                    out.push(Token {
                        tok: Tok::Template(parts),
                        line,
                    });
                }
                b'/' if regex_allowed(out.last()) => {
                    self.skip_regex()?;
                    out.push(Token {
                        tok: Tok::Regex,
                        line,
                    });
                }
                b'0'..=b'9' => {
                    let start = self.pos;
                    while self.peek(0).is_some_and(|b| is_ident_byte(b) || b == b'.') {
                        self.pos += 1;
                    }
                    let raw = String::from_utf8_lossy(&self.src[start..self.pos]).into_owned();
                    out.push(Token {
                        tok: Tok::Num(raw),
                        line,
                    });
                }
                b if is_ident_byte(b) || b >= 0x80 => {
                    let start = self.pos;
                    while self.peek(0).is_some_and(|b| is_ident_byte(b) || b >= 0x80) {
                        self.pos += 1;
                    }
                    let raw = String::from_utf8_lossy(&self.src[start..self.pos]).into_owned();
                    out.push(Token {
                        tok: Tok::Ident(raw),
                        line,
                    });
                }
                b'{' => {
                    depth += 1;
                    self.pos += 1;
                    out.push(Token {
                        tok: Tok::Punct("{".into()),
                        line,
                    });
                }
                b'}' => {
                    self.pos += 1;
                    if in_template && depth == 0 {
                        return Ok(out);
                    }
                    depth = depth.saturating_sub(1);
                    out.push(Token {
                        tok: Tok::Punct("}".into()),
                        line,
                    });
                }
                _ => {
                    let rest = &self.src[self.pos..];
                    let punct = PUNCTS
                        .iter()
                        .find(|p| rest.starts_with(p.as_bytes()))
                        .map(|p| p.to_string())
                        .unwrap_or_else(|| (byte as char).to_string());
                    self.pos += punct.len();
                    out.push(Token {
                        tok: Tok::Punct(punct),
                        line,
                    });
                }
            }
        }
        if in_template {
            return Err(format!(
                "line {}: unterminated template expression",
                self.line
            ));
        }
        Ok(out)
    }

    fn lex_escape(&mut self, buf: &mut Vec<u8>) {
        // `self.pos` is on the backslash.
        self.pos += 1;
        let Some(escaped) = self.peek(0) else { return };
        self.pos += 1;
        let simple = match escaped {
            b'n' => Some(b'\n'),
            b't' => Some(b'\t'),
            b'r' => Some(b'\r'),
            b'b' => Some(0x08),
            b'f' => Some(0x0c),
            b'v' => Some(0x0b),
            b'0' => Some(0),
            _ => None,
        };
        if let Some(byte) = simple {
            buf.push(byte);
            return;
        }
        match escaped {
            b'\n' => self.line += 1,
            b'\r' => {
                if self.peek(0) == Some(b'\n') {
                    self.pos += 1;
                }
                self.line += 1;
            }
            b'x' | b'u' => {
                let (digits, braced) = if escaped == b'u' && self.peek(0) == Some(b'{') {
                    self.pos += 1;
                    let start = self.pos;
                    while self.peek(0).is_some_and(|b| b != b'}') {
                        self.pos += 1;
                    }
                    let digits = self.src[start..self.pos].to_vec();
                    self.pos += 1;
                    (digits, true)
                } else {
                    let len = if escaped == b'x' { 2 } else { 4 };
                    let end = (self.pos + len).min(self.src.len());
                    let digits = self.src[self.pos..end].to_vec();
                    self.pos = end;
                    (digits, false)
                };
                let decoded = std::str::from_utf8(&digits)
                    .ok()
                    .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                    .and_then(char::from_u32);
                match decoded {
                    Some(ch) => buf.extend_from_slice(ch.to_string().as_bytes()),
                    None if braced => {}
                    None => buf.extend_from_slice(&digits),
                }
            }
            other => buf.push(other),
        }
    }

    fn lex_string(&mut self, quote: u8) -> Result<String, String> {
        let start_line = self.line;
        self.pos += 1;
        let mut buf = Vec::new();
        loop {
            match self.peek(0) {
                None | Some(b'\n') => {
                    return Err(format!("line {start_line}: unterminated string literal"));
                }
                Some(b'\\') => self.lex_escape(&mut buf),
                Some(byte) if byte == quote => {
                    self.pos += 1;
                    return Ok(String::from_utf8_lossy(&buf).into_owned());
                }
                Some(byte) => {
                    buf.push(byte);
                    self.pos += 1;
                }
            }
        }
    }

    fn lex_template(&mut self) -> Result<Vec<TplTok>, String> {
        let start_line = self.line;
        self.pos += 1;
        let mut parts = Vec::new();
        let mut buf = Vec::new();
        loop {
            match self.peek(0) {
                None => return Err(format!("line {start_line}: unterminated template literal")),
                Some(b'`') => {
                    self.pos += 1;
                    break;
                }
                Some(b'\\') => self.lex_escape(&mut buf),
                Some(b'$') if self.peek(1) == Some(b'{') => {
                    self.pos += 2;
                    if !buf.is_empty() {
                        parts.push(TplTok::Text(String::from_utf8_lossy(&buf).into_owned()));
                        buf.clear();
                    }
                    parts.push(TplTok::Expr(self.lex_tokens(true)?));
                }
                Some(byte) => {
                    if byte == b'\n' {
                        self.line += 1;
                    }
                    buf.push(byte);
                    self.pos += 1;
                }
            }
        }
        if !buf.is_empty() {
            parts.push(TplTok::Text(String::from_utf8_lossy(&buf).into_owned()));
        }
        Ok(parts)
    }

    fn skip_regex(&mut self) -> Result<(), String> {
        let start_line = self.line;
        self.pos += 1;
        let mut in_class = false;
        loop {
            match self.peek(0) {
                None | Some(b'\n') => {
                    return Err(format!("line {start_line}: unterminated regex literal"));
                }
                Some(b'\\') => self.pos += 2,
                Some(b'[') => {
                    in_class = true;
                    self.pos += 1;
                }
                Some(b']') => {
                    in_class = false;
                    self.pos += 1;
                }
                Some(b'/') if !in_class => {
                    self.pos += 1;
                    break;
                }
                Some(_) => self.pos += 1,
            }
        }
        while self.peek(0).is_some_and(|b| b.is_ascii_alphabetic()) {
            self.pos += 1;
        }
        Ok(())
    }
}

fn regex_allowed(previous: Option<&Token>) -> bool {
    match previous.map(|token| &token.tok) {
        None => true,
        Some(Tok::Punct(p)) => p != ")" && p != "]",
        Some(Tok::Ident(word)) => REGEX_KEYWORDS.contains(&word.as_str()),
        Some(_) => false,
    }
}

#[derive(Debug, Clone)]
enum Expr {
    Str(String),
    Num(String),
    /// Identifier plus the file-level token index it was read at.
    Ident(String, usize),
    Template(Vec<TplExpr>),
    Binary(String, Box<Expr>, Box<Expr>),
    Cond(Box<Expr>, Box<Expr>),
    Call(Box<Expr>, Vec<Expr>),
    Member(Box<Expr>, String),
    Array(Vec<Expr>),
    /// A function or arrow; keeps the string literals of its body.
    Func(Vec<String>),
    Other,
}

#[derive(Debug, Clone)]
enum TplExpr {
    Text(String),
    Expr(Expr),
}

struct Parser<'t> {
    toks: &'t [Token],
    i: usize,
    /// Template sub-parsers report the enclosing template token's index.
    fixed_pos: Option<usize>,
}

fn binary_precedence(op: &str) -> Option<u8> {
    Some(match op {
        "??" | "||" => 1,
        "&&" => 2,
        "|" => 3,
        "^" => 4,
        "&" => 5,
        "==" | "!=" | "===" | "!==" => 6,
        "<" | ">" | "<=" | ">=" | "instanceof" | "in" => 7,
        "<<" | ">>" | ">>>" => 8,
        "+" | "-" => 9,
        "*" | "/" | "%" => 10,
        "**" => 11,
        _ => return None,
    })
}

impl<'t> Parser<'t> {
    fn new(toks: &'t [Token], start: usize) -> Self {
        Parser {
            toks,
            i: start,
            fixed_pos: None,
        }
    }

    fn tok(&self) -> Option<&'t Tok> {
        self.toks.get(self.i).map(|token| &token.tok)
    }

    fn is_punct(&self, punct: &str) -> bool {
        matches!(self.tok(), Some(Tok::Punct(p)) if p == punct)
    }

    fn expect(&mut self, punct: &str) -> Result<(), String> {
        if self.is_punct(punct) {
            self.i += 1;
            Ok(())
        } else {
            Err(format!("expected `{punct}` at token {}", self.i))
        }
    }

    fn pos(&self) -> usize {
        self.fixed_pos.unwrap_or(self.i)
    }

    /// Skips a balanced `open ... close` run starting on `open`; returns its string literals.
    fn skip_balanced(&mut self, open: &str, close: &str) -> Result<Vec<String>, String> {
        let mut depth = 0usize;
        let mut literals = Vec::new();
        while let Some(tok) = self.tok() {
            match tok {
                Tok::Punct(p) if p == open => depth += 1,
                Tok::Punct(p) if p == close => {
                    depth -= 1;
                    if depth == 0 {
                        self.i += 1;
                        return Ok(literals);
                    }
                }
                Tok::Str(value) => literals.push(value.clone()),
                _ => {}
            }
            self.i += 1;
        }
        Err(format!("unbalanced `{open}`"))
    }

    fn parse_assign(&mut self) -> Result<Expr, String> {
        let lhs = self.parse_cond()?;
        if let Some(Tok::Punct(p)) = self.tok()
            && (p == "=" || (p.len() >= 2 && p.ends_with('=') && !p.starts_with('=')))
            && !matches!(p.as_str(), "==" | "!=" | "<=" | ">=" | "===" | "!==")
        {
            self.i += 1;
            self.parse_assign()?;
            return Ok(Expr::Other);
        }
        Ok(lhs)
    }

    fn parse_cond(&mut self) -> Result<Expr, String> {
        let condition = self.parse_binary(1)?;
        if !self.is_punct("?") {
            return Ok(condition);
        }
        self.i += 1;
        let consequent = self.parse_assign()?;
        self.expect(":")?;
        let alternate = self.parse_assign()?;
        Ok(Expr::Cond(Box::new(consequent), Box::new(alternate)))
    }

    fn parse_binary(&mut self, min_precedence: u8) -> Result<Expr, String> {
        let mut lhs = self.parse_unary()?;
        loop {
            let op = match self.tok() {
                Some(Tok::Punct(p)) => p.clone(),
                Some(Tok::Ident(w)) if w == "instanceof" || w == "in" => w.clone(),
                _ => break,
            };
            let Some(precedence) = binary_precedence(&op) else {
                break;
            };
            if precedence < min_precedence {
                break;
            }
            self.i += 1;
            let rhs = self.parse_binary(precedence + 1)?;
            lhs = Expr::Binary(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self) -> Result<Expr, String> {
        let prefix = match self.tok() {
            Some(Tok::Punct(p)) => matches!(p.as_str(), "!" | "-" | "+" | "~" | "++" | "--"),
            Some(Tok::Ident(w)) => {
                matches!(w.as_str(), "typeof" | "void" | "delete" | "await" | "new")
            }
            _ => false,
        };
        if prefix {
            self.i += 1;
            self.parse_unary()?;
            return Ok(Expr::Other);
        }
        self.parse_postfix()
    }

    fn parse_postfix(&mut self) -> Result<Expr, String> {
        let mut expr = self.parse_primary()?;
        loop {
            if self.is_punct(".") || self.is_punct("?.") {
                self.i += 1;
                match self.tok() {
                    Some(Tok::Ident(name)) => {
                        let name = name.clone();
                        self.i += 1;
                        expr = Expr::Member(Box::new(expr), name);
                    }
                    _ if self.is_punct("(") || self.is_punct("[") => {}
                    _ => return Err(format!("expected property name at token {}", self.i)),
                }
            } else if self.is_punct("(") {
                self.i += 1;
                let mut args = Vec::new();
                while !self.is_punct(")") {
                    if self.is_punct("...") {
                        self.i += 1;
                    }
                    args.push(self.parse_assign()?);
                    if !self.is_punct(")") {
                        self.expect(",")?;
                    }
                }
                self.i += 1;
                expr = Expr::Call(Box::new(expr), args);
            } else if self.is_punct("[") {
                self.i += 1;
                self.parse_sequence()?;
                self.expect("]")?;
                expr = Expr::Other;
            } else if self.is_punct("++") || self.is_punct("--") {
                self.i += 1;
                expr = Expr::Other;
            } else {
                return Ok(expr);
            }
        }
    }

    fn parse_sequence(&mut self) -> Result<Expr, String> {
        let mut last = self.parse_assign()?;
        while self.is_punct(",") {
            self.i += 1;
            last = self.parse_assign()?;
        }
        Ok(last)
    }

    fn parse_arrow_body(&mut self) -> Result<Expr, String> {
        if self.is_punct("{") {
            return Ok(Expr::Func(self.skip_balanced("{", "}")?));
        }
        let body = self.parse_assign()?;
        let mut literals = Vec::new();
        collect_literals(&body, &mut literals);
        Ok(Expr::Func(literals))
    }

    fn parse_primary(&mut self) -> Result<Expr, String> {
        let Some(tok) = self.tok() else {
            return Err("unexpected end of input".into());
        };
        match tok {
            Tok::Str(value) => {
                self.i += 1;
                Ok(Expr::Str(value.clone()))
            }
            Tok::Num(raw) => {
                self.i += 1;
                Ok(Expr::Num(raw.clone()))
            }
            Tok::Regex => {
                self.i += 1;
                Ok(Expr::Other)
            }
            Tok::Template(parts) => {
                let pos = self.pos();
                self.i += 1;
                let mut out = Vec::new();
                for part in parts {
                    match part {
                        TplTok::Text(text) => out.push(TplExpr::Text(text.clone())),
                        TplTok::Expr(tokens) => {
                            let mut sub = Parser {
                                toks: tokens,
                                i: 0,
                                fixed_pos: Some(pos),
                            };
                            out.push(TplExpr::Expr(sub.parse_sequence()?));
                        }
                    }
                }
                Ok(Expr::Template(out))
            }
            Tok::Ident(word) if word == "function" => {
                self.i += 1;
                if self.is_punct("*") {
                    self.i += 1;
                }
                if matches!(self.tok(), Some(Tok::Ident(_))) {
                    self.i += 1;
                }
                self.skip_balanced("(", ")")?;
                Ok(Expr::Func(self.skip_balanced("{", "}")?))
            }
            Tok::Ident(word) => {
                let word = word.clone();
                let pos = self.pos();
                self.i += 1;
                if self.is_punct("=>") {
                    self.i += 1;
                    return self.parse_arrow_body();
                }
                if matches!(
                    word.as_str(),
                    "true" | "false" | "null" | "undefined" | "this"
                ) {
                    return Ok(Expr::Other);
                }
                Ok(Expr::Ident(word, pos))
            }
            Tok::Punct(p) if p == "(" => {
                let start = self.i;
                self.skip_balanced("(", ")")?;
                if self.is_punct("=>") {
                    self.i += 1;
                    return self.parse_arrow_body();
                }
                self.i = start + 1;
                let inner = self.parse_sequence()?;
                self.expect(")")?;
                Ok(inner)
            }
            Tok::Punct(p) if p == "[" => {
                self.i += 1;
                let mut elements = Vec::new();
                while !self.is_punct("]") {
                    if self.is_punct(",") {
                        self.i += 1;
                        continue;
                    }
                    if self.is_punct("...") {
                        self.i += 1;
                        self.parse_assign()?;
                        elements.push(Expr::Other);
                    } else {
                        elements.push(self.parse_assign()?);
                    }
                    if !self.is_punct("]") {
                        self.expect(",")?;
                    }
                }
                self.i += 1;
                Ok(Expr::Array(elements))
            }
            Tok::Punct(p) if p == "{" => {
                self.skip_balanced("{", "}")?;
                Ok(Expr::Other)
            }
            Tok::Punct(p) => Err(format!("unexpected `{p}` at token {}", self.i)),
        }
    }
}

fn collect_literals(expr: &Expr, out: &mut Vec<String>) {
    match expr {
        Expr::Str(value) => out.push(value.clone()),
        Expr::Func(literals) => out.extend(literals.iter().cloned()),
        Expr::Template(parts) => {
            for part in parts {
                match part {
                    TplExpr::Text(text) => out.push(text.clone()),
                    TplExpr::Expr(inner) => collect_literals(inner, out),
                }
            }
        }
        Expr::Binary(_, lhs, rhs) | Expr::Cond(lhs, rhs) => {
            collect_literals(lhs, out);
            collect_literals(rhs, out);
        }
        Expr::Call(callee, args) => {
            collect_literals(callee, out);
            for arg in args {
                collect_literals(arg, out);
            }
        }
        Expr::Member(object, _) => collect_literals(object, out),
        Expr::Array(elements) => {
            for element in elements {
                collect_literals(element, out);
            }
        }
        Expr::Num(_) | Expr::Ident(..) | Expr::Other => {}
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum DeclKind {
    Init,
    Assign,
    Append,
}

#[derive(Debug)]
struct Decl {
    name: String,
    kind: DeclKind,
    /// Token index of the declared name, and the first index past its value.
    pos: usize,
    end: usize,
    expr: Expr,
}

#[derive(Debug)]
struct FuncDef {
    file: usize,
    params: Vec<String>,
    returns: Vec<Expr>,
}

/// A function body (token range, exclusive) and its parameter names.
#[derive(Debug)]
struct Scope {
    start: usize,
    end: usize,
    params: Vec<String>,
}

struct PolicyFile {
    rel_path: String,
    tokens: Vec<Token>,
    decls: Vec<Decl>,
    scopes: Vec<Scope>,
}

impl PolicyFile {
    fn innermost_scope(&self, pos: usize) -> Option<&Scope> {
        self.scopes
            .iter()
            .filter(|scope| scope.start < pos && pos < scope.end)
            .min_by_key(|scope| scope.end - scope.start)
    }

    /// A declaration is visible where its enclosing function body also encloses the use.
    fn visible(&self, decl_pos: usize, use_pos: usize) -> bool {
        match self.innermost_scope(decl_pos) {
            None => true,
            Some(scope) => scope.start < use_pos && use_pos < scope.end,
        }
    }

    fn shadowing_param(&self, name: &str, use_pos: usize) -> Option<&Scope> {
        self.scopes
            .iter()
            .filter(|scope| scope.start < use_pos && use_pos < scope.end)
            .filter(|scope| scope.params.iter().any(|param| param == name))
            .min_by_key(|scope| scope.end - scope.start)
    }
}

fn ident_at<'a>(tokens: &'a [Token], index: usize) -> Option<&'a str> {
    match tokens.get(index).map(|token| &token.tok) {
        Some(Tok::Ident(word)) => Some(word.as_str()),
        _ => None,
    }
}

fn punct_at(tokens: &[Token], index: usize, punct: &str) -> bool {
    matches!(tokens.get(index).map(|token| &token.tok), Some(Tok::Punct(p)) if p == punct)
}

fn matching_close(tokens: &[Token], open_index: usize, open: &str, close: &str) -> Option<usize> {
    let mut depth = 0usize;
    for (offset, token) in tokens[open_index..].iter().enumerate() {
        match &token.tok {
            Tok::Punct(p) if p == open => depth += 1,
            Tok::Punct(p) if p == close => {
                depth -= 1;
                if depth == 0 {
                    return Some(open_index + offset);
                }
            }
            _ => {}
        }
    }
    None
}

fn param_names(tokens: &[Token], open: usize, close: usize) -> Vec<String> {
    (open + 1..close)
        .filter(|&index| punct_at(tokens, index - 1, "(") || punct_at(tokens, index - 1, ","))
        .filter_map(|index| ident_at(tokens, index).map(str::to_string))
        .collect()
}

fn collect_scopes(tokens: &[Token]) -> Vec<Scope> {
    let mut scopes = Vec::new();
    for index in 0..tokens.len() {
        let (params, body_open) = if ident_at(tokens, index) == Some("function") {
            let mut cursor = index + 1;
            if punct_at(tokens, cursor, "*") {
                cursor += 1;
            }
            if ident_at(tokens, cursor).is_some() {
                cursor += 1;
            }
            if !punct_at(tokens, cursor, "(") {
                continue;
            }
            let Some(close) = matching_close(tokens, cursor, "(", ")") else {
                continue;
            };
            (param_names(tokens, cursor, close), close + 1)
        } else if punct_at(tokens, index, "=>") && index > 0 {
            let params = if let Some(name) = ident_at(tokens, index - 1) {
                vec![name.to_string()]
            } else if punct_at(tokens, index - 1, ")") {
                let mut depth = 0usize;
                let mut open = None;
                for back in (0..index).rev() {
                    if punct_at(tokens, back, ")") {
                        depth += 1;
                    } else if punct_at(tokens, back, "(") {
                        depth -= 1;
                        if depth == 0 {
                            open = Some(back);
                            break;
                        }
                    }
                }
                open.map(|open| param_names(tokens, open, index - 1))
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            (params, index + 1)
        } else {
            continue;
        };
        if !punct_at(tokens, body_open, "{") {
            continue;
        }
        if let Some(end) = matching_close(tokens, body_open, "{", "}") {
            scopes.push(Scope {
                start: body_open,
                end,
                params,
            });
        }
    }
    scopes
}

fn parse_decl_value(tokens: &[Token], start: usize) -> (Expr, usize) {
    let mut parser = Parser::new(tokens, start);
    match parser.parse_assign() {
        Ok(expr) => (expr, parser.i),
        Err(_) => (Expr::Other, start),
    }
}

fn collect_decls(tokens: &[Token]) -> Vec<Decl> {
    let mut decls = Vec::new();
    for index in 0..tokens.len() {
        let Some(name) = ident_at(tokens, index) else {
            continue;
        };
        let previous = index.checked_sub(1).and_then(|prev| tokens.get(prev));
        let after_dot =
            matches!(previous.map(|t| &t.tok), Some(Tok::Punct(p)) if p == "." || p == "?.");
        if after_dot {
            continue;
        }
        let declared = matches!(
            index.checked_sub(1).and_then(|prev| ident_at(tokens, prev)),
            Some("var" | "let" | "const")
        ) || (matches!(previous.map(|t| &t.tok), Some(Tok::Punct(p)) if p == ",")
            && decls
                .last()
                .is_some_and(|d: &Decl| d.kind == DeclKind::Init && d.end + 1 == index));
        let kind = if punct_at(tokens, index + 1, "=") {
            if declared {
                DeclKind::Init
            } else {
                DeclKind::Assign
            }
        } else if punct_at(tokens, index + 1, "+=") {
            DeclKind::Append
        } else {
            continue;
        };
        let (expr, end) = parse_decl_value(tokens, index + 2);
        decls.push(Decl {
            name: name.to_string(),
            kind,
            pos: index,
            end,
            expr,
        });
    }
    decls
}

fn collect_funcs(file: usize, tokens: &[Token], funcs: &mut HashMap<String, Vec<FuncDef>>) {
    for index in 0..tokens.len() {
        if ident_at(tokens, index) != Some("function") {
            continue;
        }
        let Some(name) = ident_at(tokens, index + 1) else {
            continue;
        };
        if !punct_at(tokens, index + 2, "(") {
            continue;
        }
        let mut cursor = index + 3;
        let mut params = Vec::new();
        while cursor < tokens.len() && !punct_at(tokens, cursor, ")") {
            if let Some(param) = ident_at(tokens, cursor)
                && (punct_at(tokens, cursor - 1, "(") || punct_at(tokens, cursor - 1, ","))
            {
                params.push(param.to_string());
            }
            cursor += 1;
        }
        cursor += 1;
        if !punct_at(tokens, cursor, "{") {
            continue;
        }
        let mut depth = 0usize;
        let mut returns = Vec::new();
        while cursor < tokens.len() {
            match &tokens[cursor].tok {
                Tok::Punct(p) if p == "{" => depth += 1,
                Tok::Punct(p) if p == "}" => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                Tok::Ident(word) if word == "return" && depth == 1 => {
                    let (expr, _) = parse_decl_value(tokens, cursor + 1);
                    returns.push(expr);
                }
                _ => {}
            }
            cursor += 1;
        }
        funcs.entry(name.to_string()).or_default().push(FuncDef {
            file,
            params,
            returns,
        });
    }
}

struct Evaluator<'a> {
    files: &'a [PolicyFile],
    funcs: &'a HashMap<String, Vec<FuncDef>>,
    /// Module-level literal constants by name, when unique across files.
    consts: HashMap<String, String>,
}

type Env = HashMap<String, Vec<String>>;

fn dynamic(name: &str) -> Vec<String> {
    vec![format!("{DYN_MARKER}{name}*/")]
}

fn cartesian(lhs: Vec<String>, rhs: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for left in &lhs {
        for right in rhs {
            let joined = format!("{left}{right}");
            if !out.contains(&joined) {
                out.push(joined);
            }
        }
    }
    out.truncate(MAX_VARIANTS);
    out
}

fn union(mut lhs: Vec<String>, rhs: Vec<String>) -> Vec<String> {
    for value in rhs {
        if !lhs.contains(&value) {
            lhs.push(value);
        }
    }
    lhs.truncate(MAX_VARIANTS);
    lhs
}

impl Evaluator<'_> {
    fn eval(&self, expr: &Expr, file: usize, env: &Env, depth: usize) -> Vec<String> {
        if depth > MAX_DEPTH {
            return dynamic("depth");
        }
        match expr {
            Expr::Str(value) => vec![value.clone()],
            Expr::Num(raw) => vec![raw.clone()],
            Expr::Template(parts) => {
                let mut acc = vec![String::new()];
                for part in parts {
                    let piece = match part {
                        TplExpr::Text(text) => vec![text.clone()],
                        TplExpr::Expr(inner) => self.eval(inner, file, env, depth + 1),
                    };
                    acc = cartesian(acc, &piece);
                }
                acc
            }
            Expr::Binary(op, lhs, rhs) => match op.as_str() {
                "+" => {
                    let rhs = self.eval(rhs, file, env, depth + 1);
                    cartesian(self.eval(lhs, file, env, depth + 1), &rhs)
                }
                "||" | "??" => union(
                    self.eval(lhs, file, env, depth + 1),
                    self.eval(rhs, file, env, depth + 1),
                ),
                _ => dynamic(op),
            },
            Expr::Cond(consequent, alternate) => union(
                self.eval(consequent, file, env, depth + 1),
                self.eval(alternate, file, env, depth + 1),
            ),
            Expr::Ident(name, pos) => match env.get(name) {
                Some(values) => values.clone(),
                None => self.resolve_var(name, file, *pos, env, depth + 1),
            },
            Expr::Member(_, property) => self
                .consts
                .get(property)
                .map(|value| vec![value.clone()])
                .unwrap_or_else(|| dynamic(property)),
            Expr::Call(callee, args) => self.eval_call(callee, args, file, env, depth + 1),
            Expr::Array(_) => dynamic("array"),
            Expr::Func(_) => dynamic("function"),
            Expr::Other => dynamic("expr"),
        }
    }

    fn resolve_var(
        &self,
        name: &str,
        file: usize,
        pos: usize,
        env: &Env,
        depth: usize,
    ) -> Vec<String> {
        let source = &self.files[file];
        let shadow = source.shadowing_param(name, pos);
        let decls: Vec<&Decl> = source
            .decls
            .iter()
            .filter(|decl| decl.name == name && source.visible(decl.pos, pos))
            .filter(|decl| {
                shadow.is_none_or(|scope| scope.start < decl.pos && decl.pos < scope.end)
            })
            .collect();
        if decls.is_empty() && shadow.is_some() {
            return dynamic(name);
        }
        let before = |kind: DeclKind| {
            decls
                .iter()
                .filter(|decl| decl.kind == kind && decl.end <= pos)
                .max_by_key(|decl| decl.pos)
                .copied()
        };
        let base = before(DeclKind::Init)
            .or_else(|| before(DeclKind::Assign))
            .or_else(|| {
                decls
                    .iter()
                    .find(|decl| decl.kind == DeclKind::Init)
                    .copied()
            });
        let Some(base) = base else {
            return self
                .consts
                .get(name)
                .map(|value| vec![value.clone()])
                .unwrap_or_else(|| dynamic(name));
        };
        let base_values = self.eval(&base.expr, file, env, depth + 1);
        let mut variants = base_values.clone();
        let mut appended = base_values;
        let mut any_append = false;
        for decl in decls
            .iter()
            .filter(|decl| decl.pos > base.pos && decl.end <= pos)
        {
            let values = self.eval(&decl.expr, file, env, depth + 1);
            match decl.kind {
                DeclKind::Append => {
                    appended = cartesian(appended, &values);
                    any_append = true;
                }
                DeclKind::Assign | DeclKind::Init => variants = union(variants, values),
            }
        }
        if any_append {
            variants = union(variants, appended);
        }
        variants
    }

    fn lookup_func(&self, callee: &Expr, file: usize) -> Option<&FuncDef> {
        let Expr::Ident(name, pos) = callee else {
            return None;
        };
        let pick = |name: &str| {
            let defs = self.funcs.get(name)?;
            defs.iter()
                .find(|def| def.file == file)
                .or_else(|| (defs.len() == 1).then(|| &defs[0]))
        };
        if let Some(def) = pick(name) {
            return Some(def);
        }
        // `var helper = lib.helper;` aliases a function declared in another module.
        let alias = self.files[file]
            .decls
            .iter()
            .find(|decl| decl.name == *name && decl.kind == DeclKind::Init && decl.end <= *pos)?;
        match &alias.expr {
            Expr::Member(_, property) => pick(property),
            _ => None,
        }
    }

    fn eval_call(
        &self,
        callee: &Expr,
        args: &[Expr],
        file: usize,
        env: &Env,
        depth: usize,
    ) -> Vec<String> {
        if let Expr::Member(object, method) = callee {
            match (object.as_ref(), method.as_str()) {
                (Expr::Array(elements), "join") => {
                    let separator = args
                        .first()
                        .and_then(|arg| self.eval(arg, file, env, depth + 1).into_iter().next())
                        .unwrap_or_else(|| ",".to_string());
                    let mut acc = vec![String::new()];
                    for (index, element) in elements.iter().enumerate() {
                        let prefix = if index == 0 { "" } else { separator.as_str() };
                        let piece: Vec<String> = self
                            .eval(element, file, env, depth + 1)
                            .into_iter()
                            .map(|value| format!("{prefix}{value}"))
                            .collect();
                        acc = cartesian(acc, &piece);
                    }
                    return acc;
                }
                (_, "join") => {
                    // `ids.map(function() { return "?"; }).join(",")` builds placeholders.
                    let mut literals = Vec::new();
                    collect_literals(object, &mut literals);
                    if !literals.is_empty() && literals.iter().all(|literal| literal == "?") {
                        return vec!["?".to_string()];
                    }
                    return dynamic("join");
                }
                (_, "trim") => {
                    return self
                        .eval(object, file, env, depth + 1)
                        .into_iter()
                        .map(|value| value.trim().to_string())
                        .collect();
                }
                _ => return dynamic(method),
            }
        }
        let Some(def) = self.lookup_func(callee, file) else {
            return dynamic("call");
        };
        if def.returns.is_empty() {
            return dynamic("call");
        }
        let mut inner_env = Env::new();
        for (index, param) in def.params.iter().enumerate() {
            let values = args
                .get(index)
                .map(|arg| self.eval(arg, file, env, depth + 1))
                .unwrap_or_else(|| dynamic(param));
            inner_env.insert(param.clone(), values);
        }
        let mut out = Vec::new();
        for returned in &def.returns {
            out = union(out, self.eval(returned, def.file, &inner_env, depth + 1));
        }
        out
    }
}

fn policy_js_files(root: &Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().map(|entry| entry.path()).collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            if path.file_name().and_then(|name| name.to_str()) != Some("__tests__") {
                policy_js_files(&path, out);
            }
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("js") {
            out.push(path);
        }
    }
}

/// Extracts every raw policy DB call under `policies_root` with its SQL variants.
pub(super) fn extract_policy_sql(policies_root: &Path) -> Vec<PolicySqlCall> {
    let repo_root = policies_root.parent().unwrap_or(policies_root);
    let mut paths = Vec::new();
    policy_js_files(policies_root, &mut paths);

    let mut calls = Vec::new();
    let mut files = Vec::new();
    for path in &paths {
        let rel_path = path
            .strip_prefix(repo_root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        let text = std::fs::read_to_string(path).unwrap_or_default();
        match lex(&text) {
            Ok(tokens) => {
                let decls = collect_decls(&tokens);
                let scopes = collect_scopes(&tokens);
                files.push(PolicyFile {
                    rel_path,
                    tokens,
                    decls,
                    scopes,
                });
            }
            Err(error) => calls.push(PolicySqlCall {
                file: rel_path,
                line: 0,
                method: String::new(),
                variants: Vec::new(),
                unsupported: Some(format!("lex error: {error}")),
            }),
        }
    }

    let mut funcs = HashMap::new();
    for (index, file) in files.iter().enumerate() {
        collect_funcs(index, &file.tokens, &mut funcs);
    }
    let mut const_candidates: HashMap<String, Vec<String>> = HashMap::new();
    for file in &files {
        for decl in file.decls.iter().filter(|decl| decl.kind == DeclKind::Init) {
            if let Expr::Str(value) | Expr::Num(value) = &decl.expr {
                const_candidates
                    .entry(decl.name.clone())
                    .or_default()
                    .push(value.clone());
            }
        }
    }
    let consts = const_candidates
        .into_iter()
        .filter(|(_, values)| values.len() == 1)
        .map(|(name, mut values)| (name, values.remove(0)))
        .collect();
    let evaluator = Evaluator {
        files: &files,
        funcs: &funcs,
        consts,
    };

    for (file_index, file) in files.iter().enumerate() {
        let tokens = &file.tokens;
        for index in 0..tokens.len() {
            if ident_at(tokens, index) != Some("agentdesk")
                || !punct_at(tokens, index + 1, ".")
                || ident_at(tokens, index + 2) != Some("db")
                || (index > 0 && punct_at(tokens, index - 1, "."))
            {
                continue;
            }
            let line = tokens[index].line;
            let method = ident_at(tokens, index + 4)
                .filter(|_| punct_at(tokens, index + 3, "."))
                .filter(|method| matches!(*method, "query" | "execute" | "queryJson"))
                .filter(|_| punct_at(tokens, index + 5, "("));
            let mut call = PolicySqlCall {
                file: file.rel_path.clone(),
                line,
                method: method.unwrap_or_default().to_string(),
                variants: Vec::new(),
                unsupported: None,
            };
            if method.is_none() {
                call.unsupported = Some("raw agentdesk.db access is not a direct call".into());
                calls.push(call);
                continue;
            }
            match Parser::new(tokens, index + 6).parse_assign() {
                Ok(sql) => {
                    call.variants = evaluator.eval(&sql, file_index, &Env::new(), 0);
                }
                Err(error) => call.unsupported = Some(format!("parse error: {error}")),
            }
            calls.push(call);
        }
    }
    calls
}

fn extract_from(sources: &[(&str, &str)]) -> Vec<PolicySqlCall> {
    let root = std::env::temp_dir().join(format!(
        "agentdesk_policy_sql_extract_{}",
        uuid::Uuid::new_v4().simple()
    ));
    let policies = root.join("policies");
    for (rel, source) in sources {
        let path = policies.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, source).unwrap();
    }
    let calls = extract_policy_sql(&policies);
    std::fs::remove_dir_all(&root).unwrap();
    calls
}

#[test]
fn expands_concatenation_ternaries_templates_and_placeholder_joins() {
    let calls = extract_from(&[(
        "a.js",
        r#"
        var TABLE = "kanban_cards";
        function f(ids, live) {
          var clause = live ? "status = 'a'" : "status = 'b'";
          var placeholders = ids.map(function() { return "?"; }).join(",");
          // a comment with agentdesk.db.query("ignored")
          var re = /\/db\//g;
          return agentdesk.db.query(
            "SELECT id FROM " + TABLE + ` WHERE ${clause} AND id IN (` + placeholders + ")",
            ids
          );
        }
        "#,
    )]);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].file, "policies/a.js");
    assert_eq!(calls[0].method, "query");
    assert_eq!(
        calls[0].variants,
        vec![
            "SELECT id FROM kanban_cards WHERE status = 'a' AND id IN (?)".to_string(),
            "SELECT id FROM kanban_cards WHERE status = 'b' AND id IN (?)".to_string(),
        ]
    );
}

#[test]
fn follows_appends_and_cross_module_helpers() {
    let calls = extract_from(&[
        (
            "lib/helpers.js",
            r#"
            var LIMIT = 50;
            function activityExpr(alias) {
              return "GREATEST(" + alias + ".created_at, " + alias + ".updated_at)";
            }
            function interval(key, fallback) {
              var val = parseInt(agentdesk.config.get(key), 10);
              if (!val) val = fallback;
              return "-" + val + " minutes";
            }
            module.exports = { activityExpr: activityExpr, interval: interval, LIMIT: LIMIT };
            "#,
        ),
        (
            "b.js",
            r#"
            var helpers = require("./lib/helpers");
            var activityExpr = helpers.activityExpr;
            var interval = helpers.interval;
            function g(where) {
              var sql = "SELECT * FROM t";
              if (where) sql += " WHERE " + activityExpr("t") + " < datetime('now', '" + interval("k", 5) + "')";
              agentdesk.db.execute(sql + " LIMIT " + helpers.LIMIT);
            }
            "#,
        ),
    ]);
    let call = calls
        .iter()
        .find(|call| call.file == "policies/b.js")
        .unwrap();
    assert_eq!(call.method, "execute");
    assert!(
        call.variants
            .contains(&"SELECT * FROM t LIMIT 50".to_string()),
        "{:?}",
        call.variants
    );
    assert!(
        call.variants.contains(
            &"SELECT * FROM t WHERE GREATEST(t.created_at, t.updated_at) < datetime('now', '-5 minutes') LIMIT 50"
                .to_string()
        ),
        "{:?}",
        call.variants
    );
}

#[test]
fn parameters_shadow_same_named_variables_of_other_functions() {
    let calls = extract_from(&[(
        "d.js",
        r#"
        function a() { var where = "id = 1"; return where; }
        function b(where) { return agentdesk.db.query("SELECT 1 FROM t WHERE " + where); }
        function c() { var where = "id = 2"; return agentdesk.db.query("SELECT 1 FROM t WHERE " + where); }
        "#,
    )]);
    assert_eq!(calls.len(), 2);
    assert_eq!(
        calls[0].variants,
        vec![format!("SELECT 1 FROM t WHERE {DYN_MARKER}where*/")]
    );
    assert_eq!(
        calls[1].variants,
        vec!["SELECT 1 FROM t WHERE id = 2".to_string()]
    );
}

#[test]
fn reports_dynamic_pieces_and_unsupported_access() {
    let calls = extract_from(&[(
        "c.js",
        r#"
        function h(sql) { return agentdesk.db.query(sql, []); }
        var db = agentdesk.db;
        agentdesk.db["execute"]("DELETE FROM t");
        "#,
    )]);
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[0].variants, vec![format!("{DYN_MARKER}sql*/")]);
    assert!(calls[1].unsupported.is_some());
    assert!(calls[2].unsupported.is_some());
}

#[test]
fn every_policy_db_call_expands_to_concrete_sql() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("policies");
    let calls = extract_policy_sql(&root);
    assert!(
        calls.len() >= 100,
        "found only {} policy db calls",
        calls.len()
    );
    let problems: Vec<String> = calls
        .iter()
        .filter_map(|call| {
            let site = format!("{}:{}", call.file, call.line);
            if let Some(reason) = &call.unsupported {
                return Some(format!("{site}: {reason}"));
            }
            if call.variants.iter().all(|sql| sql.contains(DYN_MARKER)) {
                return Some(format!("{site}: no concrete variant {:?}", call.variants));
            }
            None
        })
        .collect();
    assert!(
        problems.is_empty(),
        "policy SQL the schema check cannot expand; make the SQL static or \
         teach policy_sql_schema_tests.rs the pattern:\n{}",
        problems.join("\n")
    );
}
