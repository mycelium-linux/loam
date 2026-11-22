// SPDX-License-Identifier: MIT OR Apache-2.0
//! A small S-expression reader.
//!
//! This is intentionally not a general Lisp reader. It supports the forms the
//! Soil Lisp IR needs: lists, symbols, keywords, strings, numbers, booleans, and
//! nil. Comments begin with `;`.

use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Nil,
    Bool(bool),
    Int(i64),
    Float(f64),
    Symbol(String),
    Keyword(String),
    String(String),
    List(Vec<Value>),
}

impl Value {
    pub fn as_symbol(&self) -> Option<&str> {
        match self {
            Self::Symbol(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_keyword(&self) -> Option<&str> {
        match self {
            Self::Keyword(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_list(&self) -> Option<&[Value]> {
        match self {
            Self::List(values) => Some(values),
            // Lisp's empty list and nil are the same value.
            Self::Nil => Some(&[]),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Int(value) => Some(*value as f64),
            Self::Float(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(value) | Self::Symbol(value) | Self::Keyword(value) => Some(value),
            _ => None,
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Nil => write!(formatter, "nil"),
            Self::Bool(value) => write!(formatter, "{value}"),
            Self::Int(value) => write!(formatter, "{value}"),
            Self::Float(value) => write!(formatter, "{value}"),
            Self::Symbol(value) => write!(formatter, "{value}"),
            Self::Keyword(value) => write!(formatter, ":{value}"),
            Self::String(value) => write!(formatter, "{value:?}"),
            Self::List(values) => {
                write!(formatter, "(")?;
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        write!(formatter, " ")?;
                    }
                    write!(formatter, "{value}")?;
                }
                write!(formatter, ")")
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct ParseError {
    pub message: String,
    pub offset: usize,
}

impl fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} at byte {}", self.message, self.offset)
    }
}

impl std::error::Error for ParseError {}

/// Byte range in the source that produced a form or comment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

/// A comment retained by the document reader.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Comment {
    pub text: String,
    pub span: Span,
}

/// One lexical token, including comments. This is the formatter's view of a
/// source file: whitespace is discarded, but every comment is retained exactly
/// where it appeared.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub text: String,
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenKind {
    Open,
    Close,
    Atom,
    String,
    Comment,
    Prefix,
}

impl TokenKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Close => "close",
            Self::Atom => "atom",
            Self::String => "string",
            Self::Comment => "comment",
            Self::Prefix => "prefix",
        }
    }
}

/// Tokenize without losing comments or string spelling.
///
/// This is deliberately a second, small scanner rather than a parser mode: the
/// formatter needs trivia that the value reader deliberately drops.
pub fn tokenize(input: &str) -> Result<Vec<Token>, ParseError> {
    let mut tokens = Vec::new();
    let mut pos = 0;
    while pos < input.len() {
        let ch = input[pos..].chars().next().expect("in bounds");
        if ch.is_whitespace() {
            pos += ch.len_utf8();
            continue;
        }
        let start = pos;
        let kind = match ch {
            '(' => {
                pos += 1;
                TokenKind::Open
            }
            ')' => {
                pos += 1;
                TokenKind::Close
            }
            ';' => {
                while pos < input.len() && input.as_bytes()[pos] != b'\n' {
                    pos += 1;
                }
                TokenKind::Comment
            }
            '"' => {
                pos += 1;
                let mut escaped = false;
                while pos < input.len() {
                    let byte = input.as_bytes()[pos];
                    pos += 1;
                    if escaped {
                        escaped = false;
                    } else if byte == b'\\' {
                        escaped = true;
                    } else if byte == b'"' {
                        break;
                    }
                }
                if input.as_bytes().get(pos.saturating_sub(1)) != Some(&b'"') {
                    return Err(ParseError {
                        message: "unterminated string".into(),
                        offset: start,
                    });
                }
                TokenKind::String
            }
            '\'' | '`' => {
                pos += 1;
                TokenKind::Prefix
            }
            ',' => {
                pos += 1;
                if input.as_bytes().get(pos) == Some(&b'@') {
                    pos += 1;
                }
                TokenKind::Prefix
            }
            '#' if input.as_bytes().get(pos + 1) == Some(&b'\'') => {
                pos += 2;
                TokenKind::Prefix
            }
            _ => {
                while pos < input.len() {
                    let ch = input[pos..].chars().next().expect("in bounds");
                    if ch.is_whitespace()
                        || matches!(ch, '(' | ')' | ';' | '"' | '\'' | '`' | ',')
                    {
                        break;
                    }
                    pos += ch.len_utf8();
                }
                TokenKind::Atom
            }
        };
        tokens.push(Token {
            kind,
            text: input[start..pos].to_string(),
            start,
            end: pos,
        });
    }
    Ok(tokens)
}

/// One top-level form with its source identity.
#[derive(Clone, Debug, PartialEq)]
pub struct Form {
    pub value: Value,
    pub span: Span,
    pub comments: Vec<Comment>,
    pub id: String,
}

/// A source document that keeps top-level spans and comments.
#[derive(Clone, Debug, PartialEq)]
pub struct Document {
    pub source: String,
    pub forms: Vec<Form>,
    pub trailing_comments: Vec<Comment>,
}

impl Document {
    pub fn form_at(&self, offset: usize) -> Option<&Form> {
        self.forms
            .iter()
            .find(|form| form.span.start <= offset && offset < form.span.end)
    }

    pub fn form_by_id(&self, id: &str) -> Option<&Form> {
        self.forms.iter().find(|form| form.id == id)
    }

    /// Byte-for-byte original source.
    pub fn print_original(&self) -> &str {
        &self.source
    }
}

/// Read top-level forms while retaining their spans and leading comments.
///
/// Nested comments are still consumed by the value reader; this is the first
/// lossless slice. The canonical printer is lossless for top-level comments.
pub fn parse_document(input: &str) -> Result<Document, ParseError> {
    let mut parser = Parser {
        bytes: input.as_bytes(),
        pos: 0,
    };
    let mut forms = Vec::new();
    let mut pending = Vec::new();

    loop {
        parser.skip_ws_collect(&mut pending);
        if parser.pos >= parser.bytes.len() {
            break;
        }
        let start = parser.pos;
        let value = parser.parse_value()?;
        let end = parser.pos;
        forms.push(Form {
            id: form_id(&value, forms.len()),
            value,
            span: Span { start, end },
            comments: std::mem::take(&mut pending),
        });
    }

    Ok(Document {
        source: input.to_string(),
        forms,
        trailing_comments: pending,
    })
}

/// Canonical top-level printer. Comments are emitted before their form.
pub fn print_document(document: &Document) -> String {
    let mut output = String::new();
    for form in &document.forms {
        for comment in &form.comments {
            output.push_str(&comment.text);
            output.push('\n');
        }
        output.push_str(&form.value.to_string());
        output.push('\n');
    }
    for comment in &document.trailing_comments {
        output.push_str(&comment.text);
        output.push('\n');
    }
    output
}

fn form_id(value: &Value, index: usize) -> String {
    let Some(items) = value.as_list() else {
        return format!("form:{index}");
    };
    let head = items.first().and_then(Value::as_symbol);
    let name = items.get(1).and_then(Value::as_symbol);
    match (head, name) {
        (Some("defun" | "def" | "defmacro" | "defspore"), Some(name)) => {
            format!("{}:{name}", head.unwrap())
        }
        (Some(head), _) => format!("{head}:{index}"),
        _ => format!("form:{index}"),
    }
}

pub fn parse_all(input: &str) -> Result<Vec<Value>, ParseError> {
    let mut parser = Parser {
        bytes: input.as_bytes(),
        pos: 0,
    };
    let mut values = Vec::new();

    loop {
        parser.skip_ws();
        if parser.pos >= parser.bytes.len() {
            break;
        }
        values.push(parser.parse_value()?);
    }

    Ok(values)
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn error(&self, message: impl Into<String>) -> ParseError {
        ParseError {
            message: message.into(),
            offset: self.pos,
        }
    }

    fn skip_ws(&mut self) {
        self.skip_ws_collect(&mut Vec::new());
    }

    fn skip_ws_collect(&mut self, comments: &mut Vec<Comment>) {
        while let Some(&byte) = self.bytes.get(self.pos) {
            if byte == b';' {
                let start = self.pos;
                while let Some(&comment_byte) = self.bytes.get(self.pos) {
                    self.pos += 1;
                    if comment_byte == b'\n' {
                        break;
                    }
                }
                comments.push(Comment {
                    text: String::from_utf8_lossy(&self.bytes[start..self.pos])
                        .trim_end()
                        .to_string(),
                    span: Span {
                        start,
                        end: self.pos,
                    },
                });
            } else if byte.is_ascii_whitespace() {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn parse_value(&mut self) -> Result<Value, ParseError> {
        self.skip_ws();
        let Some(&byte) = self.bytes.get(self.pos) else {
            return Err(self.error("unexpected end of input"));
        };

        match byte {
            b'(' => self.parse_list(),
            b')' => Err(self.error("unexpected `)`")),
            b'\'' => {
                self.pos += 1;
                let value = self.parse_value()?;
                Ok(Value::List(vec![Value::Symbol("quote".to_string()), value]))
            }
            b'#' if self.bytes.get(self.pos + 1) == Some(&b'\'') => {
                self.pos += 2;
                let value = self.parse_value()?;
                Ok(Value::List(vec![
                    Value::Symbol("function".to_string()),
                    value,
                ]))
            }
            b'`' => {
                self.pos += 1;
                let value = self.parse_value()?;
                Ok(Value::List(vec![
                    Value::Symbol("quasiquote".to_string()),
                    value,
                ]))
            }
            b',' => {
                self.pos += 1;
                let splicing = self.bytes.get(self.pos) == Some(&b'@');
                if splicing {
                    self.pos += 1;
                }
                let value = self.parse_value()?;
                Ok(Value::List(vec![
                    Value::Symbol(
                        if splicing {
                            "unquote-splicing"
                        } else {
                            "unquote"
                        }
                        .to_string(),
                    ),
                    value,
                ]))
            }
            b'"' => self.parse_string(),
            _ => self.parse_atom(),
        }
    }

    fn parse_list(&mut self) -> Result<Value, ParseError> {
        self.pos += 1;
        let mut values = Vec::new();

        loop {
            self.skip_ws();
            let Some(&byte) = self.bytes.get(self.pos) else {
                return Err(self.error("unterminated list"));
            };
            if byte == b')' {
                self.pos += 1;
                return Ok(if values.is_empty() {
                    Value::Nil
                } else {
                    Value::List(values)
                });
            }
            values.push(self.parse_value()?);
        }
    }

    fn parse_string(&mut self) -> Result<Value, ParseError> {
        self.pos += 1;
        let mut value = String::new();

        loop {
            let Some(&byte) = self.bytes.get(self.pos) else {
                return Err(self.error("unterminated string"));
            };
            match byte {
                b'"' => {
                    self.pos += 1;
                    return Ok(Value::String(value));
                }
                b'\\' => {
                    self.pos += 1;
                    let Some(&escaped) = self.bytes.get(self.pos) else {
                        return Err(self.error("unterminated escape"));
                    };
                    match escaped {
                        b'n' => value.push('\n'),
                        b'r' => value.push('\r'),
                        b't' => value.push('\t'),
                        b'"' => value.push('"'),
                        b'\\' => value.push('\\'),
                        other => value.push(other as char),
                    }
                    self.pos += 1;
                }
                _ => {
                    let rest = std::str::from_utf8(&self.bytes[self.pos..])
                        .map_err(|_| self.error("invalid UTF-8"))?;
                    let character = rest
                        .chars()
                        .next()
                        .ok_or_else(|| self.error("unexpected end of string"))?;
                    value.push(character);
                    self.pos += character.len_utf8();
                }
            }
        }
    }

    fn parse_atom(&mut self) -> Result<Value, ParseError> {
        let start = self.pos;
        while let Some(&byte) = self.bytes.get(self.pos) {
            if is_delimiter(byte) {
                break;
            }
            self.pos += 1;
        }

        if start == self.pos {
            return Err(self.error("expected an atom"));
        }

        let atom = std::str::from_utf8(&self.bytes[start..self.pos])
            .map_err(|_| self.error("invalid UTF-8"))?;

        match atom {
            "nil" => Ok(Value::Nil),
            "true" | "#t" => Ok(Value::Bool(true)),
            "false" | "#f" => Ok(Value::Bool(false)),
            _ if atom.starts_with(':') => Ok(Value::Keyword(atom[1..].to_string())),
            _ => {
                if let Ok(value) = atom.parse::<i64>() {
                    Ok(Value::Int(value))
                } else if let Ok(value) = atom.parse::<f64>() {
                    Ok(Value::Float(value))
                } else {
                    Ok(Value::Symbol(atom.to_string()))
                }
            }
        }
    }
}

fn is_delimiter(byte: u8) -> bool {
    byte.is_ascii_whitespace() || matches!(byte, b'(' | b')' | b'"' | b';' | b'`' | b',' | b'\'')
}
