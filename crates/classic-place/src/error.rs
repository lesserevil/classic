//! Shared parse-pipeline error type. The lexer and parser produce
//! `ParseError`s with `kind` discriminating the stage; the type checker
//! reuses the same envelope under `Type`. The full `Display` impl that
//! renders the user-visible "line N col M: ..." form arrives with the
//! golden-error task (classic-r33).

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub msg: String,
    /// 1-indexed line of the offending token / span start.
    pub line: u32,
    /// 1-indexed column.
    pub col: u32,
    /// Token kinds the parser expected at this position; empty when the
    /// error is not "wanted X, got Y" shaped.
    pub expected: Vec<&'static str>,
    pub kind: ParseErrorKind,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ParseErrorKind {
    /// Lexer rejection — bad escape, malformed literal, unterminated
    /// string, source over the size cap, etc.
    Lex,
    /// Parser rejection — token sequence doesn't match the grammar.
    Syntax,
    /// Type-checker rejection — well-formed AST, wrong types or wrong
    /// shape (e.g. predicate when rank required).
    Type,
}

impl ParseError {
    pub fn lex(msg: impl Into<String>, line: u32, col: u32) -> Self {
        Self {
            msg: msg.into(),
            line,
            col,
            expected: Vec::new(),
            kind: ParseErrorKind::Lex,
        }
    }
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "line {} col {}: {}", self.line, self.col, self.msg)
    }
}

impl std::error::Error for ParseError {}
