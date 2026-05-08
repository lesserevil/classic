//! Hand-written recursive-descent parser for the placement DSL. Consumes
//! the lexer's `Token` stream and produces an `Expr` AST.
//!
//! Precedence is encoded in the call chain (low → high): or → and → not →
//! cmp → add → mul → unary → primary.

use crate::ast::{AggOp, BinOp, Expr, UnaryOp};
use crate::error::{ParseError, ParseErrorKind};
use crate::lex::{Pos, TokKind, Token};

/// Parse a complete expression from `toks`. Trailing tokens are an error.
pub fn parse_expr(toks: &[Token]) -> Result<Expr, ParseError> {
    let mut p = Parser { toks, i: 0 };
    let expr = p.parse_or()?;
    if p.peek().is_some() {
        let pos = p.peek_pos();
        return Err(syntax_err(
            "unexpected token after expression",
            &[],
            pos,
        ));
    }
    Ok(expr)
}

struct Parser<'a> {
    toks: &'a [Token],
    i: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&Token> {
        self.toks.get(self.i)
    }
    fn peek_kind(&self) -> Option<&TokKind> {
        self.peek().map(|t| &t.kind)
    }
    fn peek_pos(&self) -> Pos {
        self.peek()
            .map(|t| t.pos)
            .unwrap_or(Pos { line: 1, col: 1, byte: 0 })
    }
    fn bump(&mut self) -> &'a Token {
        let t = &self.toks[self.i];
        self.i += 1;
        t
    }

    fn parse_or(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_and()?;
        while matches!(self.peek_kind(), Some(TokKind::OrOr)) {
            self.bump();
            let rhs = self.parse_and()?;
            lhs = Expr::BinOp(BinOp::Or, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_and(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_not()?;
        while matches!(self.peek_kind(), Some(TokKind::AndAnd)) {
            self.bump();
            let rhs = self.parse_not()?;
            lhs = Expr::BinOp(BinOp::And, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_not(&mut self) -> Result<Expr, ParseError> {
        if matches!(self.peek_kind(), Some(TokKind::Bang)) {
            self.bump();
            let inner = self.parse_not()?;
            return Ok(Expr::UnaryOp(UnaryOp::Not, Box::new(inner)));
        }
        self.parse_cmp()
    }

    fn parse_cmp(&mut self) -> Result<Expr, ParseError> {
        let lhs = self.parse_add()?;
        // `x in [..]` lowers to In(x, [..])
        if matches!(self.peek_kind(), Some(TokKind::KwIn)) {
            self.bump();
            let list_pos = self.peek_pos();
            if !matches!(self.peek_kind(), Some(TokKind::LBracket)) {
                return Err(syntax_err(
                    "`in` must be followed by a `[..]` list",
                    &["["],
                    list_pos,
                ));
            }
            let items = self.parse_list_body()?;
            return Ok(Expr::In(Box::new(lhs), items));
        }
        // At most one comparison; chained comparisons are a parse error.
        let cmp_op = match self.peek_kind() {
            Some(TokKind::EqEq) => Some(BinOp::Eq),
            Some(TokKind::BangEq) => Some(BinOp::Ne),
            Some(TokKind::Lt) => Some(BinOp::Lt),
            Some(TokKind::LtEq) => Some(BinOp::Le),
            Some(TokKind::Gt) => Some(BinOp::Gt),
            Some(TokKind::GtEq) => Some(BinOp::Ge),
            _ => None,
        };
        let Some(op) = cmp_op else {
            return Ok(lhs);
        };
        self.bump();
        let rhs = self.parse_add()?;
        // Reject a second comparison-level operator: `a < b < c`.
        if matches!(
            self.peek_kind(),
            Some(TokKind::EqEq | TokKind::BangEq | TokKind::Lt | TokKind::LtEq | TokKind::Gt | TokKind::GtEq)
        ) {
            let pos = self.peek_pos();
            return Err(ParseError {
                msg: "comparison operators are not associative".into(),
                line: pos.line,
                col: pos.col,
                expected: vec![],
                kind: ParseErrorKind::Syntax,
            });
        }
        Ok(Expr::BinOp(op, Box::new(lhs), Box::new(rhs)))
    }

    fn parse_add(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_mul()?;
        loop {
            let op = match self.peek_kind() {
                Some(TokKind::Plus) => BinOp::Add,
                Some(TokKind::Minus) => BinOp::Sub,
                _ => break,
            };
            self.bump();
            let rhs = self.parse_mul()?;
            lhs = Expr::BinOp(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_mul(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_unary()?;
        loop {
            let op = match self.peek_kind() {
                Some(TokKind::Star) => BinOp::Mul,
                Some(TokKind::Slash) => BinOp::Div,
                _ => break,
            };
            self.bump();
            let rhs = self.parse_unary()?;
            lhs = Expr::BinOp(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self) -> Result<Expr, ParseError> {
        if matches!(self.peek_kind(), Some(TokKind::Minus)) {
            self.bump();
            let inner = self.parse_unary()?;
            return Ok(Expr::UnaryOp(UnaryOp::Neg, Box::new(inner)));
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<Expr, ParseError> {
        let pos = self.peek_pos();
        let Some(tok) = self.peek() else {
            return Err(syntax_err(
                "unexpected end of input",
                &["expression"],
                pos,
            ));
        };
        match &tok.kind {
            TokKind::LParen => {
                self.bump();
                let inner = self.parse_or()?;
                if !matches!(self.peek_kind(), Some(TokKind::RParen)) {
                    return Err(syntax_err(
                        "expected `)`",
                        &[")"],
                        self.peek_pos(),
                    ));
                }
                self.bump();
                Ok(inner)
            }
            TokKind::LBracket => {
                // Bare list literal. The grammar only allows lists as the
                // RHS of `in`, but parse it anyway and let the type checker
                // (or our `In` lowering above) decide.
                let items = self.parse_list_body()?;
                Ok(Expr::List(items))
            }
            TokKind::Int(v) => {
                let v = *v;
                self.bump();
                Ok(Expr::Int(v))
            }
            TokKind::Float(v) => {
                let v = *v;
                self.bump();
                Ok(Expr::Float(v))
            }
            TokKind::Str(_) => {
                let s = if let TokKind::Str(s) = &self.bump().kind {
                    s.clone()
                } else {
                    unreachable!()
                };
                Ok(Expr::Str(s))
            }
            TokKind::KwTrue => {
                self.bump();
                Ok(Expr::Bool(true))
            }
            TokKind::KwFalse => {
                self.bump();
                Ok(Expr::Bool(false))
            }
            TokKind::KwAny | TokKind::KwAll | TokKind::KwCount => {
                self.parse_aggregate()
            }
            TokKind::KwIn => Err(reserved_keyword_err("in", pos)),
            TokKind::Ident(_) => self.parse_field_path(),
            other => Err(syntax_err(
                &format!("unexpected token in primary position: {:?}", other),
                &[],
                pos,
            )),
        }
    }

    fn parse_aggregate(&mut self) -> Result<Expr, ParseError> {
        let head = self.bump();
        let op = match &head.kind {
            TokKind::KwAny => AggOp::Any,
            TokKind::KwAll => AggOp::All,
            TokKind::KwCount => AggOp::Count,
            _ => unreachable!(),
        };
        let head_pos = head.pos;
        // An aggregate keyword not followed by `(` is the "reserved keyword
        // as identifier" error. Plan §"Errors you might see" gives a stylized
        // example `any.in == 3` blaming `in` at col 5 — we report the head
        // keyword instead at col 1, which is the more honest blame: the
        // mistake is using `any` outside aggregate syntax.
        if !matches!(self.peek_kind(), Some(TokKind::LParen)) {
            let kw = match op {
                AggOp::Any => "any",
                AggOp::All => "all",
                AggOp::Count => "count",
            };
            return Err(reserved_keyword_err(kw, head_pos));
        }
        self.bump(); // (

        // First arg: collection name (an Ident).
        let coll_pos = self.peek_pos();
        let coll = match self.peek_kind() {
            Some(TokKind::Ident(s)) => s.clone(),
            Some(TokKind::KwIn | TokKind::KwAny | TokKind::KwAll | TokKind::KwCount | TokKind::KwTrue | TokKind::KwFalse) => {
                let kw = self.bump_kw_str();
                return Err(reserved_keyword_err(kw, coll_pos));
            }
            _ => {
                return Err(syntax_err(
                    "expected collection name in aggregate",
                    &["identifier"],
                    coll_pos,
                ));
            }
        };
        self.bump();

        let pred = if matches!(self.peek_kind(), Some(TokKind::Comma)) {
            self.bump();
            Some(Box::new(self.parse_or()?))
        } else {
            None
        };
        if !matches!(self.peek_kind(), Some(TokKind::RParen)) {
            return Err(syntax_err(
                "expected `,` or `)` in aggregate",
                &[",", ")"],
                self.peek_pos(),
            ));
        }
        self.bump();
        Ok(Expr::Agg(op, coll, pred))
    }

    /// Caller has already verified the next token is a (real) Ident.
    fn parse_field_path(&mut self) -> Result<Expr, ParseError> {
        let mut segs = Vec::new();
        match &self.peek().unwrap().kind {
            TokKind::Ident(s) => {
                segs.push(s.clone());
                self.bump();
            }
            _ => unreachable!(),
        }
        loop {
            if !matches!(self.peek_kind(), Some(TokKind::Dot)) {
                break;
            }
            self.bump();
            let seg_pos = self.peek_pos();
            match self.peek_kind() {
                Some(TokKind::Ident(s)) => {
                    segs.push(s.clone());
                    self.bump();
                }
                Some(
                    TokKind::KwIn
                    | TokKind::KwAny
                    | TokKind::KwAll
                    | TokKind::KwCount
                    | TokKind::KwTrue
                    | TokKind::KwFalse,
                ) => {
                    let kw = self.bump_kw_str();
                    return Err(reserved_keyword_err(kw, seg_pos));
                }
                _ => {
                    return Err(syntax_err(
                        "expected field name after `.`",
                        &["identifier"],
                        seg_pos,
                    ));
                }
            }
        }
        Ok(Expr::Field(segs))
    }

    fn parse_list_body(&mut self) -> Result<Vec<Expr>, ParseError> {
        // Caller has verified the LBracket is current.
        self.bump(); // [
        let mut out = Vec::new();
        if matches!(self.peek_kind(), Some(TokKind::RBracket)) {
            self.bump();
            return Ok(out);
        }
        loop {
            out.push(self.parse_or()?);
            match self.peek_kind() {
                Some(TokKind::Comma) => {
                    self.bump();
                }
                Some(TokKind::RBracket) => {
                    self.bump();
                    return Ok(out);
                }
                _ => {
                    return Err(syntax_err(
                        "expected `,` or `]` in list",
                        &[",", "]"],
                        self.peek_pos(),
                    ));
                }
            }
        }
    }

    fn bump_kw_str(&mut self) -> &'static str {
        let s = match self.peek_kind() {
            Some(TokKind::KwAny) => "any",
            Some(TokKind::KwAll) => "all",
            Some(TokKind::KwCount) => "count",
            Some(TokKind::KwIn) => "in",
            Some(TokKind::KwTrue) => "true",
            Some(TokKind::KwFalse) => "false",
            _ => "<keyword>",
        };
        self.bump();
        s
    }
}

fn syntax_err(msg: &str, expected: &[&'static str], pos: Pos) -> ParseError {
    ParseError {
        msg: msg.to_string(),
        line: pos.line,
        col: pos.col,
        expected: expected.to_vec(),
        kind: ParseErrorKind::Syntax,
    }
}

fn reserved_keyword_err(kw: &str, pos: Pos) -> ParseError {
    ParseError {
        msg: format!("`{}` is a reserved keyword and cannot be used as a field name", kw),
        line: pos.line,
        col: pos.col,
        expected: vec![],
        kind: ParseErrorKind::Syntax,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lex::lex;

    fn parse(src: &str) -> Result<Expr, ParseError> {
        parse_expr(&lex(src).unwrap())
    }

    #[test]
    fn or_and_precedence() {
        // a || b && c  ==  a || (b && c)
        let e = parse("a || b && c").unwrap();
        match e {
            Expr::BinOp(BinOp::Or, lhs, rhs) => {
                assert!(matches!(*lhs, Expr::Field(_)));
                assert!(matches!(*rhs, Expr::BinOp(BinOp::And, _, _)));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn not_wraps_full_comparison() {
        // Per the plan grammar, NotExpr := "!" NotExpr | CmpExpr — so `!`
        // is *lower* precedence than comparison: `!a == b` parses as
        // `!(a == b)`.
        let e = parse("!a == b").unwrap();
        match e {
            Expr::UnaryOp(UnaryOp::Not, inner) => {
                assert!(matches!(*inner, Expr::BinOp(BinOp::Eq, _, _)));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn unary_minus_then_add() {
        // -a + b  ==  (-a) + b
        let e = parse("-a + b").unwrap();
        match e {
            Expr::BinOp(BinOp::Add, lhs, _) => assert!(matches!(*lhs, Expr::UnaryOp(UnaryOp::Neg, _))),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn chained_comparison_rejected() {
        let err = parse("cpu.cores < 32 < 64").unwrap_err();
        assert_eq!(err.kind, ParseErrorKind::Syntax);
        assert!(err.msg.contains("not associative"));
    }

    #[test]
    fn count_arity_one_and_two() {
        match parse("count(gpu)").unwrap() {
            Expr::Agg(AggOp::Count, var, None) => assert_eq!(var, "gpu"),
            other => panic!("{other:?}"),
        }
        match parse("count(gpu, gpu.in_use)").unwrap() {
            Expr::Agg(AggOp::Count, var, Some(_)) => assert_eq!(var, "gpu"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn any_with_predicate() {
        let e = parse("any(gpu, gpu.vram_mb >= 80000)").unwrap();
        match e {
            Expr::Agg(AggOp::Any, coll, Some(pred)) => {
                assert_eq!(coll, "gpu");
                assert!(matches!(*pred, Expr::BinOp(BinOp::Ge, _, _)));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn in_lowers_to_in_node() {
        let e = parse("x in [1, 2, 3]").unwrap();
        match e {
            Expr::In(lhs, items) => {
                assert!(matches!(*lhs, Expr::Field(_)));
                assert_eq!(items.len(), 3);
                assert!(matches!(items[0], Expr::Int(1)));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn hex_decimal_float_distinct() {
        match parse("0x10de").unwrap() {
            Expr::Int(v) => assert_eq!(v, 0x10de),
            other => panic!("{other:?}"),
        }
        match parse("50").unwrap() {
            Expr::Int(50) => {}
            other => panic!("{other:?}"),
        }
        match parse("50.0").unwrap() {
            Expr::Float(f) => assert!((f - 50.0).abs() < 1e-9),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn reserved_keyword_used_as_aggregate_head_outside_aggregate_rejected() {
        // `any.in == 3` — `any` is in primary position but not followed by
        // `(`. Our parser blames the head keyword.
        let err = parse("any.in == 3").unwrap_err();
        assert_eq!(err.kind, ParseErrorKind::Syntax);
        assert!(err.msg.contains("reserved keyword"), "msg: {}", err.msg);
    }

    #[test]
    fn keyword_after_dot_rejected() {
        // `gpu.in` — `in` is reserved; can't be a member name.
        let err = parse("gpu.in").unwrap_err();
        assert_eq!(err.kind, ParseErrorKind::Syntax);
        assert!(err.msg.contains("reserved keyword"));
        // Position should point at `in`, after the `.`.
        assert!(err.col > 1);
    }

    #[test]
    fn trailing_tokens_rejected() {
        let err = parse("1 + 1 garbage").unwrap_err();
        assert_eq!(err.kind, ParseErrorKind::Syntax);
        assert!(err.msg.contains("unexpected"));
    }

    #[test]
    fn unbalanced_paren_rejected() {
        let err = parse("(1 + 2").unwrap_err();
        assert_eq!(err.kind, ParseErrorKind::Syntax);
    }

    #[test]
    fn unbalanced_bracket_in_list_rejected() {
        let err = parse("x in [1, 2").unwrap_err();
        assert_eq!(err.kind, ParseErrorKind::Syntax);
    }

    #[test]
    fn full_example_predicate() {
        // Plan example 1.
        let e = parse("any(gpu, gpu.vendor == 0x10de && gpu.vram_mb >= 80000 && !gpu.in_use)").unwrap();
        match e {
            Expr::Agg(AggOp::Any, coll, Some(_)) => assert_eq!(coll, "gpu"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parse_error_carries_line_col() {
        // Force an error on line 2.
        let err = parse("a < 32\n< 64").unwrap_err();
        assert_eq!(err.line, 2);
    }
}
