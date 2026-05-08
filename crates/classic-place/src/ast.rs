//! AST for the placement predicate DSL. Mirrors plan-03 §"AST" exactly so
//! the type checker and evaluator can match-and-recurse without translation.

#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    /// Only legal as the RHS of `in`. Surfaces as a parse error if it
    /// appears anywhere else.
    List(Vec<Expr>),
    /// Field path like `["gpu", "vram_mb"]`. Resolution + arity (per-element
    /// vs scalar) is the type checker's job.
    Field(Vec<String>),
    BinOp(BinOp, Box<Expr>, Box<Expr>),
    UnaryOp(UnaryOp, Box<Expr>),
    /// `x in [a, b, c]` — already lowered from the surface syntax.
    In(Box<Expr>, Vec<Expr>),
    /// `any/all/count(coll [, pred])` — `coll` is the iteration-variable
    /// name (also the root collection name; the binding shadows the same
    /// outer field-path head).
    Agg(AggOp, String, Option<Box<Expr>>),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BinOp {
    Or,
    And,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Add,
    Sub,
    Mul,
    Div,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum UnaryOp {
    Not,
    Neg,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AggOp {
    Any,
    All,
    Count,
}

/// Type-checked predicate expression. Constructed by the type checker
/// from an `Expr` once it has verified the result type is bool.
#[derive(Clone, Debug)]
pub struct Requirement(pub Expr);

/// Type-checked rank expression — numeric (int or float) result.
#[derive(Clone, Debug)]
pub struct Rank(pub Expr);
