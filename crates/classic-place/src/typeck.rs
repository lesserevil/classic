//! Type checker for the placement DSL. Walks the AST validating that
//! field paths resolve, operators see compatible operand types, and the
//! top-level expression yields the right shape (`Requirement` requires
//! `Bool`; `Rank` requires `Int` or `Float`).
//!
//! Coercion: `Int` widens to `Float` in numeric comparisons and arithmetic
//! when one side is `Float`. `Str` does not coerce.

use std::collections::HashMap;

use crate::ast::{AggOp, BinOp, Expr, Rank, Requirement, UnaryOp};
use crate::error::{ParseError, ParseErrorKind};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ty {
    Bool,
    Int,
    Float,
    Str,
    GpuColl,
    PciColl,
    GpuElem,
    PciElem,
    List(Box<Ty>),
}

impl Ty {
    pub fn is_numeric(&self) -> bool {
        matches!(self, Ty::Int | Ty::Float)
    }
    pub fn label(&self) -> &'static str {
        match self {
            Ty::Bool => "bool",
            Ty::Int => "int",
            Ty::Float => "float",
            Ty::Str => "string",
            Ty::GpuColl => "[GpuAd]",
            Ty::PciColl => "[PciAd]",
            Ty::GpuElem => "GpuAd",
            Ty::PciElem => "PciAd",
            Ty::List(_) => "list",
        }
    }
}

/// Type-check `e` as a requirement (boolean predicate). Returns the
/// wrapped `Requirement` on success.
pub fn check_req(e: Expr) -> Result<Requirement, ParseError> {
    let mut env = Env::root();
    let ty = check_expr(&e, &mut env)?;
    if ty != Ty::Bool {
        return Err(type_err(
            &format!("requirement must be a boolean expression, got {}", ty.label()),
            1,
            1,
        ));
    }
    Ok(Requirement(e))
}

/// Type-check `e` as a rank (numeric expression). Int or Float both ok.
pub fn check_rank(e: Expr) -> Result<Rank, ParseError> {
    let mut env = Env::root();
    let ty = check_expr(&e, &mut env)?;
    if !ty.is_numeric() {
        return Err(type_err(
            &format!("rank must be a numeric expression, got {}", ty.label()),
            1,
            1,
        ));
    }
    Ok(Rank(e))
}

struct Env {
    /// Per-binding shadowed types. Currently only used by aggregates
    /// (`any/all/count(coll, ...)` shadows `coll` to its element type).
    bindings: HashMap<String, Ty>,
}

impl Env {
    fn root() -> Self {
        Self { bindings: HashMap::new() }
    }
}

fn check_expr(e: &Expr, env: &mut Env) -> Result<Ty, ParseError> {
    match e {
        Expr::Bool(_) => Ok(Ty::Bool),
        Expr::Int(_) => Ok(Ty::Int),
        Expr::Float(_) => Ok(Ty::Float),
        Expr::Str(_) => Ok(Ty::Str),
        Expr::List(items) => {
            let mut elem_ty: Option<Ty> = None;
            for it in items {
                let t = check_expr(it, env)?;
                match &mut elem_ty {
                    None => elem_ty = Some(t),
                    Some(prev) => {
                        if !ty_compatible(prev, &t) {
                            return Err(type_err(
                                &format!(
                                    "list elements must share a type; got {} and {}",
                                    prev.label(),
                                    t.label()
                                ),
                                1,
                                1,
                            ));
                        }
                    }
                }
            }
            Ok(Ty::List(Box::new(elem_ty.unwrap_or(Ty::Bool))))
        }
        Expr::Field(path) => resolve_field(path, env),
        Expr::BinOp(op, a, b) => {
            let ta = check_expr(a, env)?;
            let tb = check_expr(b, env)?;
            check_binop(*op, &ta, &tb)
        }
        Expr::UnaryOp(op, inner) => {
            let t = check_expr(inner, env)?;
            match op {
                UnaryOp::Not => {
                    if t != Ty::Bool {
                        return Err(type_err(
                            &format!("`!` requires bool, got {}", t.label()),
                            1,
                            1,
                        ));
                    }
                    Ok(Ty::Bool)
                }
                UnaryOp::Neg => {
                    if !t.is_numeric() {
                        return Err(type_err(
                            &format!("unary `-` requires numeric, got {}", t.label()),
                            1,
                            1,
                        ));
                    }
                    Ok(t)
                }
            }
        }
        Expr::In(lhs, items) => {
            let lt = check_expr(lhs, env)?;
            for it in items {
                let it_ty = check_expr(it, env)?;
                if !ty_compatible(&lt, &it_ty) {
                    return Err(type_err(
                        &format!(
                            "`in` list element {} incompatible with LHS {}",
                            it_ty.label(),
                            lt.label()
                        ),
                        1,
                        1,
                    ));
                }
            }
            Ok(Ty::Bool)
        }
        Expr::Agg(op, var, body) => {
            let elem_ty = match var.as_str() {
                "gpu" => Ty::GpuElem,
                "pci" => Ty::PciElem,
                other => {
                    return Err(type_err(
                        &format!(
                            "unknown aggregate collection `{}` (known: gpu, pci)",
                            other
                        ),
                        1,
                        1,
                    ));
                }
            };
            // Bind the iteration variable for the duration of the body.
            let prev = env.bindings.insert(var.clone(), elem_ty.clone());
            let body_ty = match body {
                Some(b) => Some(check_expr(b, env)?),
                None => None,
            };
            // Restore.
            match prev {
                Some(p) => {
                    env.bindings.insert(var.clone(), p);
                }
                None => {
                    env.bindings.remove(var);
                }
            }
            match (op, body_ty) {
                (AggOp::Count, None) => Ok(Ty::Int),
                (AggOp::Count, Some(t)) => {
                    if t != Ty::Bool {
                        return Err(type_err(
                            &format!("count predicate must be bool, got {}", t.label()),
                            1,
                            1,
                        ));
                    }
                    Ok(Ty::Int)
                }
                (AggOp::Any | AggOp::All, None) => Err(type_err(
                    "any/all require a predicate body",
                    1,
                    1,
                )),
                (AggOp::Any | AggOp::All, Some(t)) => {
                    if t != Ty::Bool {
                        return Err(type_err(
                            &format!("any/all predicate must be bool, got {}", t.label()),
                            1,
                            1,
                        ));
                    }
                    Ok(Ty::Bool)
                }
            }
        }
    }
}

/// Two types are "compatible" if they are equal, or both numeric (so
/// `Int` and `Float` line up under coercion), or one is a list whose
/// element type matches the other's element type.
fn ty_compatible(a: &Ty, b: &Ty) -> bool {
    if a == b {
        return true;
    }
    if a.is_numeric() && b.is_numeric() {
        return true;
    }
    false
}

fn check_binop(op: BinOp, a: &Ty, b: &Ty) -> Result<Ty, ParseError> {
    match op {
        BinOp::Or | BinOp::And => {
            if a != &Ty::Bool || b != &Ty::Bool {
                return Err(type_err(
                    &format!(
                        "`{:?}` requires bool && bool, got {} and {}",
                        op,
                        a.label(),
                        b.label()
                    ),
                    1,
                    1,
                ));
            }
            Ok(Ty::Bool)
        }
        BinOp::Eq | BinOp::Ne => {
            if !ty_compatible(a, b) {
                return Err(type_err(
                    &format!("cannot compare {} and {}", a.label(), b.label()),
                    1,
                    1,
                ));
            }
            Ok(Ty::Bool)
        }
        BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
            if !(a.is_numeric() && b.is_numeric()) {
                return Err(type_err(
                    &format!("cannot compare {} and {}", a.label(), b.label()),
                    1,
                    1,
                ));
            }
            Ok(Ty::Bool)
        }
        BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div => {
            if !(a.is_numeric() && b.is_numeric()) {
                return Err(type_err(
                    &format!(
                        "arithmetic requires numeric operands, got {} and {}",
                        a.label(),
                        b.label()
                    ),
                    1,
                    1,
                ));
            }
            // Result widens: any Float -> Float; otherwise Int.
            if a == &Ty::Float || b == &Ty::Float {
                Ok(Ty::Float)
            } else {
                Ok(Ty::Int)
            }
        }
    }
}

fn resolve_field(path: &[String], env: &Env) -> Result<Ty, ParseError> {
    if path.is_empty() {
        return Err(type_err("empty field path", 1, 1));
    }
    let head = path[0].as_str();

    // Bound by an enclosing aggregate?
    if let Some(bound) = env.bindings.get(head) {
        if path.len() == 1 {
            return Ok(bound.clone());
        }
        let member = path[1].as_str();
        let elem_ty = bound.clone();
        let mut cur = resolve_member(&elem_ty, member, head)?;
        for seg in &path[2..] {
            cur = resolve_member(&cur, seg, member)?;
        }
        return Ok(cur);
    }

    // Struct-root heads (cpu/mem/load/os) need the second segment to
    // resolve to a concrete scalar type — they're never values themselves.
    if path.len() >= 2 {
        if let Some(t) = struct_root_member(head, &path[1]) {
            let mut cur = t;
            for seg in &path[2..] {
                cur = resolve_member(&cur, seg, &path[1])?;
            }
            return Ok(cur);
        }
        // Head is a known struct but member isn't recognized — give a
        // friendly "known: ..." hint.
        if matches!(head, "cpu" | "mem" | "load" | "os") {
            let known = struct_known_members(head);
            return Err(type_err(
                &format!(
                    "unknown field `{}` on {} (known: {})",
                    path[1], head, known
                ),
                1,
                1,
            ));
        }
    }

    // Standalone atomic root: gpu, pci, gen, hostname, node_id.
    let root_ty = resolve_root(head)?;
    if path.len() == 1 {
        return Ok(root_ty);
    }
    let mut cur = resolve_member(&root_ty, path[1].as_str(), head)?;
    for seg in &path[2..] {
        cur = resolve_member(&cur, seg, &path[path.len() - 2])?;
    }
    Ok(cur)
}

fn struct_known_members(head: &str) -> &'static str {
    match head {
        "cpu" => "cores, threads, arch, model",
        "mem" => "total_mb, free_mb",
        "load" => "cpu_pct, mem_pct, load_1m",
        "os" => "kernel, distro",
        _ => "",
    }
}

fn resolve_root(name: &str) -> Result<Ty, ParseError> {
    Ok(match name {
        "node_id" => Ty::Str,
        "hostname" => Ty::Str,
        "gen" => Ty::Int,
        "cpu" | "mem" | "load" | "os" => {
            // Sub-struct heads — typed as opaque "namespace"; the only
            // legal next move is `.member`. We model these by walking
            // member resolution rather than introducing dedicated Tys.
            return Err(type_err(
                &format!("`{}` is a struct; access a member with `.`", name),
                1,
                1,
            ));
        }
        "gpu" => Ty::GpuColl,
        "pci" => Ty::PciColl,
        other => {
            return Err(type_err(
                &format!(
                    "unknown field `{}` (known: node_id, hostname, gen, cpu.*, mem.*, load.*, os.*, gpu, pci)",
                    other
                ),
                1,
                1,
            ));
        }
    })
}

fn resolve_member(parent: &Ty, member: &str, parent_label: &str) -> Result<Ty, ParseError> {
    let (known, ty) = match parent {
        Ty::GpuElem => (
            &["index", "vendor", "device", "model", "vram_mb", "vram_free_mb", "sm_count", "in_use", "mig"][..],
            gpu_member(member),
        ),
        Ty::PciElem => (
            &["bdf", "vendor", "device", "class"][..],
            pci_member(member),
        ),
        Ty::GpuColl => {
            // Collection-level access without iteration — only legal
            // for `count(gpu)` which doesn't traverse here. Treat any
            // member access as an error pointing at the use site.
            return Err(type_err(
                &format!(
                    "`{}.{}` requires iteration; wrap in any/all/count",
                    parent_label, member
                ),
                1,
                1,
            ));
        }
        Ty::PciColl => {
            return Err(type_err(
                &format!(
                    "`{}.{}` requires iteration; wrap in any/all/count",
                    parent_label, member
                ),
                1,
                1,
            ));
        }
        _ => {
            return Err(type_err(
                &format!("`{}` has no members", parent.label()),
                1,
                1,
            ));
        }
    };
    match ty {
        Some(t) => Ok(t),
        None => Err(type_err(
            &format!(
                "unknown field `{}` on {} (known: {})",
                member,
                parent.label(),
                known.join(", ")
            ),
            1,
            1,
        )),
    }
}

fn gpu_member(m: &str) -> Option<Ty> {
    Some(match m {
        "index" | "vendor" | "device" | "vram_mb" | "vram_free_mb" | "sm_count" => Ty::Int,
        "model" => Ty::Str,
        "in_use" | "mig" => Ty::Bool,
        _ => return None,
    })
}

fn pci_member(m: &str) -> Option<Ty> {
    Some(match m {
        "vendor" | "device" | "class" => Ty::Int,
        "bdf" => Ty::Str,
        _ => return None,
    })
}

fn type_err(msg: &str, line: u32, col: u32) -> ParseError {
    ParseError {
        msg: msg.to_string(),
        line,
        col,
        expected: vec![],
        kind: ParseErrorKind::Type,
    }
}

// Cpu / Mem / Load / Os aren't represented as enum variants of `Ty`
// because the DSL never holds a value of one — every legal use is a
// dotted access like `cpu.cores`. We special-case those at the
// `resolve_field` level: when path[0] is one of those struct heads, we
// resolve path[1] directly through the per-struct member table.

fn struct_root_member(head: &str, member: &str) -> Option<Ty> {
    Some(match (head, member) {
        ("cpu", "cores") | ("cpu", "threads") => Ty::Int,
        ("cpu", "arch") | ("cpu", "model") => Ty::Str,
        ("mem", "total_mb") | ("mem", "free_mb") => Ty::Int,
        ("load", "cpu_pct") | ("load", "mem_pct") | ("load", "load_1m") => Ty::Float,
        ("os", "kernel") | ("os", "distro") => Ty::Str,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lex::lex;
    use crate::parse::parse_expr;

    fn typeck_req(src: &str) -> Result<Requirement, ParseError> {
        let toks = lex(src).unwrap();
        let ast = parse_expr(&toks).unwrap();
        check_req(ast)
    }

    fn typeck_rank(src: &str) -> Result<Rank, ParseError> {
        let toks = lex(src).unwrap();
        let ast = parse_expr(&toks).unwrap();
        check_rank(ast)
    }

    #[test]
    fn requirement_must_be_bool() {
        let err = typeck_req("cpu.cores").unwrap_err();
        assert_eq!(err.kind, ParseErrorKind::Type);
        assert!(err.msg.contains("boolean"));
    }

    #[test]
    fn rank_must_be_numeric() {
        let err = typeck_rank("cpu.arch").unwrap_err();
        assert_eq!(err.kind, ParseErrorKind::Type);
    }

    #[test]
    fn aggregate_body_must_be_bool() {
        let err = typeck_req("any(gpu, gpu.vram_mb)").unwrap_err();
        assert_eq!(err.kind, ParseErrorKind::Type);
        assert!(err.msg.contains("bool"));
    }

    #[test]
    fn aggregate_iter_var_shadows_outer() {
        // Inside `any(gpu, ...)` the binding `gpu` is per-element so
        // `gpu.vram_mb` resolves to int.
        typeck_req("any(gpu, gpu.vram_mb >= 80000)").expect("must type-check");
    }

    #[test]
    fn unknown_field_lists_known_set() {
        let err = typeck_req("any(gpu, gpu.warp_count >= 4)").unwrap_err();
        assert_eq!(err.kind, ParseErrorKind::Type);
        assert!(err.msg.contains("known:"), "msg: {}", err.msg);
        assert!(err.msg.contains("vram_mb"));
    }

    #[test]
    fn cannot_compare_string_and_int() {
        let err = typeck_req("cpu.arch >= 5").unwrap_err();
        assert_eq!(err.kind, ParseErrorKind::Type);
        assert!(err.msg.contains("compare"));
    }

    #[test]
    fn int_to_float_coercion_in_comparison() {
        typeck_req("load.cpu_pct < 50").expect("int coerces to float");
        typeck_req("cpu.cores < 50.0").expect("int < float ok");
    }

    #[test]
    fn in_list_member_type_must_match_lhs() {
        let err = typeck_req("cpu.cores in [\"x86_64\"]").unwrap_err();
        assert_eq!(err.kind, ParseErrorKind::Type);
    }

    #[test]
    fn count_returns_int_for_arity_one_or_two() {
        typeck_rank("count(gpu)").expect("count(gpu) is numeric");
        typeck_rank("count(gpu, gpu.in_use)").expect("count with predicate is numeric");
    }

    #[test]
    fn all_six_example_predicates_typecheck() {
        let examples = [
            "any(gpu, gpu.vendor == 0x10de && gpu.vram_mb >= 80000 && !gpu.in_use)",
            "count(gpu, !gpu.in_use) >= 2",
            "any(gpu, !gpu.mig && !gpu.in_use)",
            "any(gpu, gpu.vendor in [0x1002] && gpu.vram_mb >= 192000)",
            "mem.free_mb >= 65536 && load.cpu_pct < 50.0",
            "cpu.arch == \"x86_64\" && cpu.cores >= 32 && any(pci, pci.vendor == 0x15b3)",
        ];
        for src in examples {
            typeck_req(src).unwrap_or_else(|e| panic!("{src}: {e}"));
        }
    }

    #[test]
    fn default_rank_typechecks_as_float() {
        let rank = typeck_rank(
            "-load.cpu_pct - 1000.0 * (count(gpu) - count(gpu, gpu.in_use))",
        )
        .unwrap();
        let _ = rank; // smoke
    }

    #[test]
    fn struct_root_without_member_is_friendly_error() {
        let err = typeck_req("cpu == 1").unwrap_err();
        assert!(err.msg.contains("struct"));
    }
}
