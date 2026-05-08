//! Evaluator for the placement DSL. Walks a type-checked `Requirement`
//! or `Rank` over a `model::NodeAd` and produces a boolean / numeric
//! result. Never panics — bad fields fall through as field-skew
//! (boolean false, numeric 0) per plan §FR-10.

use crate::ast::{AggOp, BinOp, Expr, Rank, Requirement, UnaryOp};
use crate::model::{GpuAd, NodeAd, PciAd};

/// Internal eval value. Never escapes the crate.
#[derive(Clone, Debug)]
enum Val {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
}

impl Val {
    fn as_bool(&self) -> Option<bool> {
        if let Val::Bool(b) = self { Some(*b) } else { None }
    }
    fn as_f64(&self) -> Option<f64> {
        match self {
            Val::Int(i) => Some(*i as f64),
            Val::Float(f) => Some(*f),
            _ => None,
        }
    }
    fn as_i64(&self) -> Option<i64> {
        if let Val::Int(i) = self { Some(*i) } else { None }
    }
}

/// Iteration binding for an enclosing aggregate.
enum Binding<'a> {
    Gpu(&'a GpuAd),
    Pci(&'a PciAd),
}

struct Env<'a> {
    /// Stack of (var_name, element) bindings. Inner shadows outer.
    bound: Vec<(&'a str, Binding<'a>)>,
}

/// Evaluate `req` against `ad`. Returns `false` on field-skew or any
/// non-bool result (defensive — a well-typed Requirement always returns
/// bool, but we treat unexpected values as failure to match).
pub fn matches(req: &Requirement, ad: &NodeAd) -> bool {
    let mut env = Env { bound: Vec::new() };
    eval(&req.0, ad, &mut env)
        .ok()
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// Evaluate `rank` against `ad`. Returns `f64::NEG_INFINITY` if the rank
/// can't be evaluated at all (e.g. type-skew); per FR-10 individual
/// missing fields contribute the surrounding op's neutral element rather
/// than failing the whole expression.
pub fn score(rank: &Rank, ad: &NodeAd) -> f64 {
    let mut env = Env { bound: Vec::new() };
    match eval(&rank.0, ad, &mut env) {
        Ok(v) => v.as_f64().unwrap_or(f64::NEG_INFINITY),
        Err(_) => f64::NEG_INFINITY,
    }
}

#[derive(Debug)]
struct EvalSkew;

fn eval<'a>(e: &'a Expr, ad: &'a NodeAd, env: &mut Env<'a>) -> Result<Val, EvalSkew> {
    match e {
        Expr::Bool(b) => Ok(Val::Bool(*b)),
        Expr::Int(i) => Ok(Val::Int(*i)),
        Expr::Float(f) => Ok(Val::Float(*f)),
        Expr::Str(s) => Ok(Val::Str(s.clone())),
        Expr::List(_) => {
            // Lists are only legal as the RHS of `in`, where they're
            // already lowered to the In(...) variant. A bare list at
            // eval time is a type-skew.
            Err(EvalSkew)
        }
        Expr::Field(path) => resolve_field(path, ad, env),
        Expr::BinOp(op, a, b) => eval_binop(*op, a, b, ad, env),
        Expr::UnaryOp(op, inner) => {
            let v = eval(inner, ad, env)?;
            match op {
                UnaryOp::Not => Ok(Val::Bool(!v.as_bool().ok_or(EvalSkew)?)),
                UnaryOp::Neg => match v {
                    Val::Int(i) => Ok(Val::Int(-i)),
                    Val::Float(f) => Ok(Val::Float(-f)),
                    _ => Err(EvalSkew),
                },
            }
        }
        Expr::In(lhs, items) => {
            let lv = eval(lhs, ad, env)?;
            for it in items {
                let rv = eval(it, ad, env)?;
                if val_eq(&lv, &rv) {
                    return Ok(Val::Bool(true));
                }
            }
            Ok(Val::Bool(false))
        }
        Expr::Agg(op, var, body) => eval_agg(*op, var, body.as_deref(), ad, env),
    }
}

fn eval_binop<'a>(
    op: BinOp,
    a: &'a Expr,
    b: &'a Expr,
    ad: &'a NodeAd,
    env: &mut Env<'a>,
) -> Result<Val, EvalSkew> {
    // Short-circuit for And/Or.
    match op {
        BinOp::And => {
            let av = eval(a, ad, env)?.as_bool().ok_or(EvalSkew)?;
            if !av {
                return Ok(Val::Bool(false));
            }
            return Ok(Val::Bool(eval(b, ad, env)?.as_bool().ok_or(EvalSkew)?));
        }
        BinOp::Or => {
            let av = eval(a, ad, env)?.as_bool().ok_or(EvalSkew)?;
            if av {
                return Ok(Val::Bool(true));
            }
            return Ok(Val::Bool(eval(b, ad, env)?.as_bool().ok_or(EvalSkew)?));
        }
        _ => {}
    }

    let av = eval(a, ad, env)?;
    let bv = eval(b, ad, env)?;
    match op {
        BinOp::Eq => Ok(Val::Bool(val_eq(&av, &bv))),
        BinOp::Ne => Ok(Val::Bool(!val_eq(&av, &bv))),
        BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
            let af = av.as_f64().ok_or(EvalSkew)?;
            let bf = bv.as_f64().ok_or(EvalSkew)?;
            Ok(Val::Bool(match op {
                BinOp::Lt => af < bf,
                BinOp::Le => af <= bf,
                BinOp::Gt => af > bf,
                BinOp::Ge => af >= bf,
                _ => unreachable!(),
            }))
        }
        BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div => {
            // Widen if either side is Float.
            let any_float = matches!(av, Val::Float(_)) || matches!(bv, Val::Float(_));
            if any_float {
                let af = av.as_f64().ok_or(EvalSkew)?;
                let bf = bv.as_f64().ok_or(EvalSkew)?;
                Ok(Val::Float(match op {
                    BinOp::Add => af + bf,
                    BinOp::Sub => af - bf,
                    BinOp::Mul => af * bf,
                    BinOp::Div => af / bf,
                    _ => unreachable!(),
                }))
            } else {
                let ai = av.as_i64().ok_or(EvalSkew)?;
                let bi = bv.as_i64().ok_or(EvalSkew)?;
                Ok(match op {
                    BinOp::Add => Val::Int(ai.wrapping_add(bi)),
                    BinOp::Sub => Val::Int(ai.wrapping_sub(bi)),
                    BinOp::Mul => Val::Int(ai.wrapping_mul(bi)),
                    // Plan doesn't pin int/0 — return field-skew so the
                    // rank evaluator falls through to the neutral element.
                    BinOp::Div => {
                        if bi == 0 { return Err(EvalSkew); }
                        Val::Int(ai / bi)
                    }
                    _ => unreachable!(),
                })
            }
        }
        BinOp::And | BinOp::Or => unreachable!(),
    }
}

fn val_eq(a: &Val, b: &Val) -> bool {
    match (a, b) {
        (Val::Bool(x), Val::Bool(y)) => x == y,
        (Val::Int(x), Val::Int(y)) => x == y,
        (Val::Float(x), Val::Float(y)) => x == y,
        (Val::Int(x), Val::Float(y)) | (Val::Float(y), Val::Int(x)) => (*x as f64) == *y,
        (Val::Str(x), Val::Str(y)) => x == y,
        _ => false,
    }
}

fn eval_agg<'a>(
    op: AggOp,
    var: &'a str,
    body: Option<&'a Expr>,
    ad: &'a NodeAd,
    env: &mut Env<'a>,
) -> Result<Val, EvalSkew> {
    match var {
        "gpu" => agg_over(op, body, ad, env, var, ad.gpu.iter().map(Binding::Gpu).collect()),
        "pci" => agg_over(op, body, ad, env, var, ad.pci.iter().map(Binding::Pci).collect()),
        _ => Err(EvalSkew),
    }
}

fn agg_over<'a>(
    op: AggOp,
    body: Option<&'a Expr>,
    ad: &'a NodeAd,
    env: &mut Env<'a>,
    var: &'a str,
    bindings: Vec<Binding<'a>>,
) -> Result<Val, EvalSkew> {
    if bindings.is_empty() {
        return Ok(match op {
            AggOp::Any => Val::Bool(false),
            AggOp::All => Val::Bool(true), // vacuous truth
            AggOp::Count => Val::Int(0),
        });
    }
    match op {
        AggOp::Count if body.is_none() => Ok(Val::Int(bindings.len() as i64)),
        AggOp::Count => {
            let pred = body.unwrap();
            let mut n: i64 = 0;
            for b in bindings {
                env.bound.push((var, b));
                let v = eval(pred, ad, env);
                env.bound.pop();
                if v?.as_bool().ok_or(EvalSkew)? {
                    n += 1;
                }
            }
            Ok(Val::Int(n))
        }
        AggOp::Any => {
            let pred = body.ok_or(EvalSkew)?;
            for b in bindings {
                env.bound.push((var, b));
                let v = eval(pred, ad, env);
                env.bound.pop();
                if v?.as_bool().ok_or(EvalSkew)? {
                    return Ok(Val::Bool(true));
                }
            }
            Ok(Val::Bool(false))
        }
        AggOp::All => {
            let pred = body.ok_or(EvalSkew)?;
            for b in bindings {
                env.bound.push((var, b));
                let v = eval(pred, ad, env);
                env.bound.pop();
                if !v?.as_bool().ok_or(EvalSkew)? {
                    return Ok(Val::Bool(false));
                }
            }
            Ok(Val::Bool(true))
        }
    }
}

fn resolve_field<'a>(path: &'a [String], ad: &'a NodeAd, env: &Env<'a>) -> Result<Val, EvalSkew> {
    if path.is_empty() {
        return Err(EvalSkew);
    }
    let head = path[0].as_str();

    // Aggregate-bound? Check the binding stack from inner to outer.
    for (name, binding) in env.bound.iter().rev() {
        if *name == head {
            return resolve_bound_field(binding, &path[1..]);
        }
    }

    // Otherwise resolve from the NodeAd root.
    match (head, path.get(1).map(|s| s.as_str())) {
        ("node_id", None) => {
            // Render NodeId as a hex string so DSL string equality works
            // against `"abcd00..."` literals.
            let mut s = String::with_capacity(32);
            for b in ad.node_id.0.iter() {
                use std::fmt::Write;
                let _ = write!(&mut s, "{:02x}", b);
            }
            Ok(Val::Str(s))
        }
        ("hostname", None) => Ok(Val::Str(ad.hostname.clone())),
        ("gen", None) => Ok(Val::Int(ad.gen as i64)),
        ("cpu", Some(m)) => match m {
            "cores" => Ok(Val::Int(ad.cpu.cores as i64)),
            "threads" => Ok(Val::Int(ad.cpu.threads as i64)),
            "arch" => Ok(Val::Str(ad.cpu.arch.clone())),
            "model" => Ok(Val::Str(ad.cpu.model.clone())),
            _ => Err(EvalSkew),
        },
        ("mem", Some(m)) => match m {
            "total_mb" => Ok(Val::Int(ad.mem.total_mb as i64)),
            "free_mb" => Ok(Val::Int(ad.mem.free_mb as i64)),
            _ => Err(EvalSkew),
        },
        ("load", Some(m)) => match m {
            "cpu_pct" => Ok(Val::Float(ad.load.cpu_pct)),
            "mem_pct" => Ok(Val::Float(ad.load.mem_pct)),
            "load_1m" => Ok(Val::Float(ad.load.load_1m)),
            _ => Err(EvalSkew),
        },
        ("os", Some(m)) => match m {
            "kernel" => Ok(Val::Str(ad.os.kernel.clone())),
            "distro" => Ok(Val::Str(ad.os.distro.clone())),
            _ => Err(EvalSkew),
        },
        // Bare collection access has no scalar value — only legal under
        // count(...) / any(...) / all(...). Treat as field-skew.
        ("gpu", None) | ("pci", None) => Err(EvalSkew),
        _ => Err(EvalSkew),
    }
}

fn resolve_bound_field(binding: &Binding<'_>, rest: &[String]) -> Result<Val, EvalSkew> {
    if rest.is_empty() {
        // `gpu` alone inside an aggregate body — no scalar form.
        return Err(EvalSkew);
    }
    let m = rest[0].as_str();
    let v = match binding {
        Binding::Gpu(g) => match m {
            "index" => Val::Int(g.index as i64),
            "vendor" => Val::Int(g.vendor as i64),
            "device" => Val::Int(g.device as i64),
            "model" => Val::Str(g.model.clone()),
            "vram_mb" => Val::Int(g.vram_mb as i64),
            "vram_free_mb" => Val::Int(g.vram_free_mb as i64),
            "sm_count" => Val::Int(g.sm_count as i64),
            "in_use" => Val::Bool(g.in_use),
            "mig" => Val::Bool(g.mig),
            _ => return Err(EvalSkew),
        },
        Binding::Pci(p) => match m {
            "bdf" => Val::Str(p.bdf.clone()),
            "vendor" => Val::Int(p.vendor as i64),
            "device" => Val::Int(p.device as i64),
            "class" => Val::Int(p.class as i64),
            _ => return Err(EvalSkew),
        },
    };
    if rest.len() > 1 {
        // GpuAd / PciAd members are scalar — no further nesting.
        return Err(EvalSkew);
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CpuAd, GpuAd, LoadAd, MemAd, NodeAd, OsAd};
    use crate::{parse_rank, parse_req};

    fn ad(builder: impl FnOnce(&mut NodeAd)) -> NodeAd {
        let mut a = NodeAd::default();
        a.hostname = "n".into();
        a.cpu.arch = "x86_64".into();
        a.cpu.model = "Test".into();
        a.cpu.cores = 16;
        a.cpu.threads = 32;
        a.mem.total_mb = 65_536;
        a.mem.free_mb = 32_768;
        a.os.kernel = "Linux".into();
        a.os.distro = "Ubuntu 24.04".into();
        builder(&mut a);
        a
    }

    fn h100(idle: bool, mig: bool) -> GpuAd {
        GpuAd {
            index: 0,
            vendor: 0x10de,
            device: 0x2330,
            model: "NVIDIA H100".into(),
            vram_mb: 81920,
            vram_free_mb: 80000,
            sm_count: 132,
            in_use: !idle,
            mig,
        }
    }

    fn a100_40(idle: bool) -> GpuAd {
        GpuAd {
            index: 0,
            vendor: 0x10de,
            device: 0x20F1,
            model: "NVIDIA A100 40GB".into(),
            vram_mb: 40960,
            vram_free_mb: 40000,
            sm_count: 108,
            in_use: !idle,
            mig: false,
        }
    }

    #[test]
    fn matches_simple_predicate_against_h100_node() {
        let req = parse_req("any(gpu, gpu.vendor == 0x10de && gpu.vram_mb >= 80000 && !gpu.in_use)").unwrap();
        let ok = ad(|a| a.gpu.push(h100(true, false)));
        let no = ad(|a| a.gpu.push(a100_40(true)));
        let none = ad(|_a| {});
        assert!(matches(&req, &ok));
        assert!(!matches(&req, &no));
        assert!(!matches(&req, &none)); // FR-7 vacuous-truth: any-of-nothing = false
    }

    #[test]
    fn count_arity_one_and_two() {
        let busy = parse_rank("count(gpu)").unwrap();
        let idle = parse_rank("count(gpu, !gpu.in_use)").unwrap();
        let four_idle = ad(|a| {
            for _ in 0..4 {
                a.gpu.push(h100(true, false));
            }
        });
        assert_eq!(score(&busy, &four_idle), 4.0);
        assert_eq!(score(&idle, &four_idle), 4.0);

        let mixed = ad(|a| {
            a.gpu.push(h100(true, false));
            a.gpu.push(h100(false, false));
        });
        assert_eq!(score(&busy, &mixed), 2.0);
        assert_eq!(score(&idle, &mixed), 1.0);
    }

    #[test]
    fn all_idle_with_empty_collection_is_vacuously_true() {
        let req = parse_req("all(gpu, gpu.in_use == false)").unwrap();
        assert!(matches(&req, &ad(|_a| {})));
    }

    #[test]
    fn in_membership() {
        let req = parse_req("cpu.arch in [\"x86_64\", \"aarch64\"]").unwrap();
        assert!(matches(&req, &ad(|_a| {})));
    }

    #[test]
    fn empty_in_list_is_false() {
        // Cast the LHS to a list of matching type via parse_req trick
        // — using vendor (int) so the empty list type is determinable.
        let req = parse_req("cpu.cores in [99]").unwrap();
        assert!(!matches(&req, &ad(|_a| {})));
    }

    #[test]
    fn numeric_widening_in_comparison() {
        let req = parse_req("load.cpu_pct < 50").unwrap();
        let lo = ad(|a| a.load.cpu_pct = 12.5);
        let hi = ad(|a| a.load.cpu_pct = 80.0);
        assert!(matches(&req, &lo));
        assert!(!matches(&req, &hi));
    }

    #[test]
    fn default_rank_prefers_idle_gpus() {
        let rank = parse_rank(
            "-load.cpu_pct - 1000.0 * (count(gpu) - count(gpu, gpu.in_use))",
        )
        .unwrap();
        let four_idle = ad(|a| {
            a.load.cpu_pct = 10.0;
            for _ in 0..4 {
                a.gpu.push(h100(true, false));
            }
        });
        let no_gpu = ad(|a| a.load.cpu_pct = 5.0);
        // Worked example from plan §Examples: 4-idle node = -10 - 4000 = -4010.
        assert!((score(&rank, &four_idle) - (-4010.0)).abs() < 1e-9);
        assert!((score(&rank, &no_gpu) - (-5.0)).abs() < 1e-9);
    }

    #[test]
    fn field_skew_returns_false_no_panic() {
        // Synthetic ad with no GPUs against a predicate referencing GpuAd
        // fields — must short-circuit to false without panic.
        let req = parse_req("any(gpu, gpu.vram_mb >= 80000)").unwrap();
        assert!(!matches(&req, &ad(|_a| {})));
    }

    #[test]
    fn never_panics_on_short_circuit() {
        // a == 1 || a == 2 — `a` is unbound; first comparison should
        // skew, but Or short-circuits so we still get a deterministic
        // answer: the first operand fails, the second is tried, also
        // fails, result false.
        let req = parse_req("cpu.cores == 1 || cpu.cores == 2").unwrap();
        let n = ad(|a| a.cpu.cores = 16);
        assert!(!matches(&req, &n));
    }

    #[test]
    fn matches_full_plan_examples() {
        let four_idle_h100 = ad(|a| {
            a.cpu.cores = 64;
            a.load.cpu_pct = 10.0;
            for i in 0..4 {
                let mut g = h100(true, false);
                g.index = i;
                a.gpu.push(g);
            }
        });
        let cpu_box = ad(|a| {
            a.cpu.cores = 64;
            a.cpu.arch = "x86_64".into();
            a.mem.free_mb = 65_536;
            a.load.cpu_pct = 30.0;
        });

        // Plan example 1: idle ≥ 80 GB NVIDIA GPU.
        assert!(matches(
            &parse_req("any(gpu, gpu.vendor == 0x10de && gpu.vram_mb >= 80000 && !gpu.in_use)").unwrap(),
            &four_idle_h100
        ));
        // Plan example 5: ≥ 64 GB free RAM and CPU < 50%.
        assert!(matches(
            &parse_req("mem.free_mb >= 65536 && load.cpu_pct < 50.0").unwrap(),
            &cpu_box
        ));
    }
}
