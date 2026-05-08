//! Lexer for the placement-predicate DSL. One-pass scanner producing a
//! `Vec<Token>` with attached `Pos`. Std-only, no allocations beyond the
//! token vector and any owned literal strings.

use crate::error::ParseError;

/// Hard cap on predicate source length. Defense against pathological
/// inputs; aligned with plan §Limits ("Predicate text size cap. 64 KiB").
pub const MAX_SRC_LEN: usize = 64 * 1024;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Pos {
    /// 1-indexed line number.
    pub line: u32,
    /// 1-indexed column (in chars, not bytes — fine for ASCII source).
    pub col: u32,
    /// Byte offset from start of source.
    pub byte: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TokKind {
    // Operators
    OrOr,
    AndAnd,
    Bang,
    EqEq,
    BangEq,
    LtEq,
    GtEq,
    Lt,
    Gt,
    Plus,
    Minus,
    Star,
    Slash,
    // Punctuation
    LParen,
    RParen,
    LBracket,
    RBracket,
    Comma,
    Dot,
    // Literals
    Int(i64),
    Float(f64),
    Str(String),
    // Reserved keywords (never an Ident)
    KwAny,
    KwAll,
    KwCount,
    KwIn,
    KwTrue,
    KwFalse,
    // Identifier
    Ident(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Token {
    pub kind: TokKind,
    pub pos: Pos,
}

pub fn lex(src: &str) -> Result<Vec<Token>, ParseError> {
    if src.len() > MAX_SRC_LEN {
        return Err(ParseError::lex(
            format!("source exceeds {}-byte cap ({})", MAX_SRC_LEN, src.len()),
            1,
            1,
        ));
    }
    let mut lx = Lexer {
        bytes: src.as_bytes(),
        i: 0,
        line: 1,
        col: 1,
    };
    let mut out = Vec::new();
    while let Some(tok) = lx.next_token()? {
        out.push(tok);
    }
    Ok(out)
}

struct Lexer<'a> {
    bytes: &'a [u8],
    i: usize,
    line: u32,
    col: u32,
}

impl<'a> Lexer<'a> {
    fn pos(&self) -> Pos {
        Pos { line: self.line, col: self.col, byte: self.i as u32 }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.i).copied()
    }
    fn peek_at(&self, off: usize) -> Option<u8> {
        self.bytes.get(self.i + off).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let b = self.peek()?;
        self.i += 1;
        if b == b'\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
        Some(b)
    }

    fn skip_ws(&mut self) {
        while let Some(b) = self.peek() {
            if b == b' ' || b == b'\t' || b == b'\r' || b == b'\n' {
                self.bump();
            } else if b == b'#' {
                // Plan grammar doesn't mandate comments, but `#` lines in
                // examples suggest support is friendly. Treat `# ... \n` as
                // a comment for resilience without expanding the grammar.
                while let Some(c) = self.peek() {
                    if c == b'\n' { break; }
                    self.bump();
                }
            } else {
                break;
            }
        }
    }

    fn next_token(&mut self) -> Result<Option<Token>, ParseError> {
        self.skip_ws();
        let pos = self.pos();
        let Some(b) = self.peek() else { return Ok(None); };

        // Two-char operators first.
        match (b, self.peek_at(1)) {
            (b'|', Some(b'|')) => { self.bump(); self.bump(); return Ok(Some(Token { kind: TokKind::OrOr, pos })); }
            (b'&', Some(b'&')) => { self.bump(); self.bump(); return Ok(Some(Token { kind: TokKind::AndAnd, pos })); }
            (b'=', Some(b'=')) => { self.bump(); self.bump(); return Ok(Some(Token { kind: TokKind::EqEq, pos })); }
            (b'!', Some(b'=')) => { self.bump(); self.bump(); return Ok(Some(Token { kind: TokKind::BangEq, pos })); }
            (b'<', Some(b'=')) => { self.bump(); self.bump(); return Ok(Some(Token { kind: TokKind::LtEq, pos })); }
            (b'>', Some(b'=')) => { self.bump(); self.bump(); return Ok(Some(Token { kind: TokKind::GtEq, pos })); }
            _ => {}
        }

        // Single-char punctuation / operators.
        let single = match b {
            b'!' => Some(TokKind::Bang),
            b'<' => Some(TokKind::Lt),
            b'>' => Some(TokKind::Gt),
            b'+' => Some(TokKind::Plus),
            b'-' => Some(TokKind::Minus),
            b'*' => Some(TokKind::Star),
            b'/' => Some(TokKind::Slash),
            b'(' => Some(TokKind::LParen),
            b')' => Some(TokKind::RParen),
            b'[' => Some(TokKind::LBracket),
            b']' => Some(TokKind::RBracket),
            b',' => Some(TokKind::Comma),
            b'.' => Some(TokKind::Dot),
            _ => None,
        };
        if let Some(kind) = single {
            self.bump();
            return Ok(Some(Token { kind, pos }));
        }

        // String literal.
        if b == b'"' {
            return Ok(Some(self.lex_string(pos)?));
        }

        // Number literal.
        if b.is_ascii_digit() {
            return Ok(Some(self.lex_number(pos)?));
        }

        // Identifier / keyword.
        if b.is_ascii_alphabetic() || b == b'_' {
            return Ok(Some(self.lex_ident(pos)));
        }

        Err(ParseError::lex(
            format!("unexpected character {:?}", b as char),
            pos.line,
            pos.col,
        ))
    }

    fn lex_string(&mut self, pos: Pos) -> Result<Token, ParseError> {
        // Consume opening quote.
        self.bump();
        let mut out = String::new();
        loop {
            let Some(b) = self.peek() else {
                return Err(ParseError::lex(
                    "unterminated string literal",
                    pos.line,
                    pos.col,
                ));
            };
            if b == b'\n' {
                return Err(ParseError::lex(
                    "unterminated string literal",
                    pos.line,
                    pos.col,
                ));
            }
            if b == b'"' {
                self.bump();
                return Ok(Token { kind: TokKind::Str(out), pos });
            }
            if b == b'\\' {
                let escape_pos = self.pos();
                self.bump();
                match self.peek() {
                    Some(b'"') => { out.push('"'); self.bump(); }
                    Some(b'\\') => { out.push('\\'); self.bump(); }
                    Some(b'n') => { out.push('\n'); self.bump(); }
                    Some(b'r') => { out.push('\r'); self.bump(); }
                    Some(b't') => { out.push('\t'); self.bump(); }
                    Some(other) => {
                        return Err(ParseError::lex(
                            format!("invalid escape sequence \\{}", other as char),
                            escape_pos.line,
                            escape_pos.col,
                        ));
                    }
                    None => {
                        return Err(ParseError::lex(
                            "unterminated string literal",
                            pos.line,
                            pos.col,
                        ));
                    }
                }
                continue;
            }
            out.push(b as char);
            self.bump();
        }
    }

    fn lex_number(&mut self, pos: Pos) -> Result<Token, ParseError> {
        // Hex int?
        if self.peek() == Some(b'0') && matches!(self.peek_at(1), Some(b'x') | Some(b'X')) {
            self.bump(); // 0
            self.bump(); // x
            let start = self.i;
            while let Some(b) = self.peek() {
                if b.is_ascii_hexdigit() { self.bump(); } else { break; }
            }
            if self.i == start {
                return Err(ParseError::lex(
                    "expected hex digit after `0x`",
                    pos.line,
                    pos.col,
                ));
            }
            let txt = std::str::from_utf8(&self.bytes[start..self.i]).unwrap();
            // After hex digits, an alphanumeric continuation is malformed
            // (e.g. `0xZZ` is rejected because Z is not a hex digit; we
            // also catch `0x12abq` here).
            if let Some(b) = self.peek() {
                if b.is_ascii_alphanumeric() || b == b'_' {
                    return Err(ParseError::lex(
                        format!("invalid hex literal: unexpected {:?}", b as char),
                        self.line,
                        self.col,
                    ));
                }
            }
            let v = i64::from_str_radix(txt, 16).map_err(|e| {
                ParseError::lex(format!("invalid hex literal: {e}"), pos.line, pos.col)
            })?;
            return Ok(Token { kind: TokKind::Int(v), pos });
        }

        // Decimal int or float.
        let start = self.i;
        while let Some(b) = self.peek() {
            if b.is_ascii_digit() { self.bump(); } else { break; }
        }
        let int_end = self.i;

        // Float requires `.` followed by at least one digit. A trailing
        // `.` (like `3.`) is a lex error per the grammar.
        if self.peek() == Some(b'.') && self.peek_at(1).map_or(false, |b| b.is_ascii_digit()) {
            self.bump(); // dot
            while let Some(b) = self.peek() {
                if b.is_ascii_digit() { self.bump(); } else { break; }
            }
            if matches!(self.peek(), Some(b'e') | Some(b'E')) {
                self.bump();
                if matches!(self.peek(), Some(b'+') | Some(b'-')) { self.bump(); }
                let exp_start = self.i;
                while let Some(b) = self.peek() {
                    if b.is_ascii_digit() { self.bump(); } else { break; }
                }
                if self.i == exp_start {
                    return Err(ParseError::lex(
                        "expected digits in float exponent",
                        pos.line,
                        pos.col,
                    ));
                }
            }
            let txt = std::str::from_utf8(&self.bytes[start..self.i]).unwrap();
            let v: f64 = txt.parse().map_err(|e| {
                ParseError::lex(format!("invalid float literal: {e}"), pos.line, pos.col)
            })?;
            return Ok(Token { kind: TokKind::Float(v), pos });
        }
        if self.peek() == Some(b'.') {
            // Trailing dot like `3.` — not a valid Float per grammar.
            return Err(ParseError::lex(
                "trailing `.` is not a valid number — write `3.0` for a float",
                pos.line,
                pos.col,
            ));
        }

        let txt = std::str::from_utf8(&self.bytes[start..int_end]).unwrap();
        let v: i64 = txt.parse().map_err(|e| {
            ParseError::lex(format!("invalid integer literal: {e}"), pos.line, pos.col)
        })?;
        Ok(Token { kind: TokKind::Int(v), pos })
    }

    fn lex_ident(&mut self, pos: Pos) -> Token {
        let start = self.i;
        while let Some(b) = self.peek() {
            if b.is_ascii_alphanumeric() || b == b'_' {
                self.bump();
            } else {
                break;
            }
        }
        let s = std::str::from_utf8(&self.bytes[start..self.i]).unwrap();
        let kind = match s {
            "any" => TokKind::KwAny,
            "all" => TokKind::KwAll,
            "count" => TokKind::KwCount,
            "in" => TokKind::KwIn,
            "true" => TokKind::KwTrue,
            "false" => TokKind::KwFalse,
            other => TokKind::Ident(other.to_string()),
        };
        Token { kind, pos }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ParseErrorKind;

    fn kinds(src: &str) -> Vec<TokKind> {
        lex(src).unwrap().into_iter().map(|t| t.kind).collect()
    }

    #[test]
    fn every_operator_is_distinct_kind() {
        let src = "|| && ! == != <= >= < > + - * /";
        let ks = kinds(src);
        assert_eq!(
            ks,
            vec![
                TokKind::OrOr, TokKind::AndAnd, TokKind::Bang, TokKind::EqEq, TokKind::BangEq,
                TokKind::LtEq, TokKind::GtEq, TokKind::Lt, TokKind::Gt,
                TokKind::Plus, TokKind::Minus, TokKind::Star, TokKind::Slash,
            ]
        );
    }

    #[test]
    fn punctuation_each_kind() {
        assert_eq!(
            kinds("( ) [ ] , ."),
            vec![TokKind::LParen, TokKind::RParen, TokKind::LBracket, TokKind::RBracket, TokKind::Comma, TokKind::Dot]
        );
    }

    #[test]
    fn integer_literals_decimal_and_hex() {
        let toks = lex("32 0x10de 0xff").unwrap();
        assert_eq!(toks[0].kind, TokKind::Int(32));
        assert_eq!(toks[1].kind, TokKind::Int(0x10de));
        assert_eq!(toks[2].kind, TokKind::Int(0xff));
    }

    #[test]
    fn float_literals_with_optional_exponent() {
        let toks = lex("1.5 2.0e3 1.0E-2").unwrap();
        assert_eq!(toks[0].kind, TokKind::Float(1.5));
        assert_eq!(toks[1].kind, TokKind::Float(2000.0));
        assert_eq!(toks[2].kind, TokKind::Float(0.01));
    }

    #[test]
    fn string_literal_with_escapes() {
        let toks = lex(r#""H100\n" "x86_64""#).unwrap();
        match &toks[0].kind {
            TokKind::Str(s) => assert_eq!(s, "H100\n"),
            other => panic!("expected Str, got {other:?}"),
        }
        match &toks[1].kind {
            TokKind::Str(s) => assert_eq!(s, "x86_64"),
            other => panic!("expected Str, got {other:?}"),
        }
    }

    #[test]
    fn keywords_distinguished_from_idents() {
        let toks = lex("any all count in true false anyway in_use").unwrap();
        let kinds: Vec<_> = toks.into_iter().map(|t| t.kind).collect();
        assert_eq!(
            kinds,
            vec![
                TokKind::KwAny,
                TokKind::KwAll,
                TokKind::KwCount,
                TokKind::KwIn,
                TokKind::KwTrue,
                TokKind::KwFalse,
                TokKind::Ident("anyway".into()),
                TokKind::Ident("in_use".into()),
            ]
        );
    }

    #[test]
    fn position_tracking_across_newlines() {
        // "a\n  && b": && starts at line 2, col 3 (1-indexed).
        let toks = lex("a\n  && b").unwrap();
        let andand = toks.iter().find(|t| matches!(t.kind, TokKind::AndAnd)).unwrap();
        assert_eq!(andand.pos.line, 2);
        assert_eq!(andand.pos.col, 3);
    }

    #[test]
    fn unterminated_string_errors_at_opening_quote() {
        // The error-message example in plan §"Errors you might see"
        // points the caret at column 19 of `any(gpu, gpu.model == "H100`.
        let err = lex("any(gpu, gpu.model == \"H100").unwrap_err();
        assert_eq!(err.kind, ParseErrorKind::Lex);
        assert!(err.msg.contains("unterminated"), "msg = {}", err.msg);
        assert_eq!(err.line, 1);
        assert_eq!(err.col, 23); // column of the opening quote in this 0-padded source
    }

    #[test]
    fn bad_escape_rejected() {
        let err = lex(r#""\q""#).unwrap_err();
        assert_eq!(err.kind, ParseErrorKind::Lex);
        assert!(err.msg.contains("invalid escape"));
    }

    #[test]
    fn malformed_hex_rejected() {
        let err = lex("0xZZ").unwrap_err();
        assert_eq!(err.kind, ParseErrorKind::Lex);
    }

    #[test]
    fn hex_followed_by_alpha_rejected() {
        let err = lex("0x10abq").unwrap_err();
        assert_eq!(err.kind, ParseErrorKind::Lex);
    }

    #[test]
    fn trailing_decimal_dot_rejected() {
        let err = lex("3.").unwrap_err();
        assert_eq!(err.kind, ParseErrorKind::Lex);
    }

    #[test]
    fn source_over_cap_rejected_before_tokenizing() {
        let huge = "a".repeat(MAX_SRC_LEN + 1);
        let err = lex(&huge).unwrap_err();
        assert_eq!(err.kind, ParseErrorKind::Lex);
        assert!(err.msg.contains("cap"));
    }
}
