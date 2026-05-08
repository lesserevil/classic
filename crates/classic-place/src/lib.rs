pub mod ast;
pub mod error;
pub mod frame;
pub mod lex;
pub mod parse;
pub mod typeck;

pub use ast::{AggOp, BinOp, Expr, Rank, Requirement, UnaryOp};
pub use error::{ParseError, ParseErrorKind};
pub use frame::{
    PlaceErrKind, PlacedCandidate, PlacementError, PlacementRequest, PlacementResponse,
};
pub use lex::{lex, Pos, TokKind, Token, MAX_SRC_LEN};
pub use parse::parse_expr;
pub use typeck::{check_rank, check_req, Ty};

/// Parse + type-check a requirement (boolean predicate). The full parse
/// pipeline: lex → parse → type-check.
pub fn parse_req(src: &str) -> Result<Requirement, ParseError> {
    let toks = lex(src)?;
    let ast = parse_expr(&toks)?;
    check_req(ast)
}

/// Parse + type-check a rank (numeric expression).
pub fn parse_rank(src: &str) -> Result<Rank, ParseError> {
    let toks = lex(src)?;
    let ast = parse_expr(&toks)?;
    check_rank(ast)
}
