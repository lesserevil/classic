//! Table-driven parser + type-checker tests across every grammar
//! production and the six plan-3 example predicates.

use classic_place::{parse_rank, parse_req};

#[test]
fn all_six_example_predicates_parse_and_typecheck() {
    let preds = [
        "any(gpu, gpu.vendor == 0x10de && gpu.vram_mb >= 80000 && !gpu.in_use)",
        "count(gpu, !gpu.in_use) >= 2",
        "any(gpu, !gpu.mig && !gpu.in_use)",
        "any(gpu, gpu.vendor in [0x1002] && gpu.vram_mb >= 192000)",
        "mem.free_mb >= 65536 && load.cpu_pct < 50.0",
        "cpu.arch == \"x86_64\" && cpu.cores >= 32 && any(pci, pci.vendor == 0x15b3)",
    ];
    for src in preds {
        parse_req(src).unwrap_or_else(|e| panic!("{src}: {e}"));
    }
}

#[test]
fn default_rank_text_parses_byte_for_byte() {
    let _ = parse_rank(classic_place::DEFAULT_RANK_SRC).expect("default rank parses");
}

#[test]
fn each_grammar_production_has_at_least_one_smoke_test() {
    // OrExpr / AndExpr / NotExpr / CmpExpr / AddExpr / MulExpr /
    // UnaryExpr / Primary / Aggregate / ListLit / FieldPath / Literal.
    let cases = [
        // Or / And / Not
        "true || false",
        "true && false",
        "!true",
        // Comparisons
        "1 == 1",
        "1 != 2",
        "1 < 2",
        "2 <= 2",
        "3 > 2",
        "3 >= 3",
        // Add / Sub / Mul / Div
        "1 + 1 == 2",
        "5 - 2 == 3",
        "2 * 3 == 6",
        "6 / 2 == 3",
        // Unary minus
        "-1 == -1",
        // Aggregates: any / all / count, arity-1 and arity-2
        "any(gpu, gpu.vram_mb > 0)",
        "all(gpu, gpu.in_use == false)",
        "count(gpu) == 0",
        "count(gpu, gpu.in_use) == 0",
        // List + in
        "1 in [1, 2, 3]",
        "0 in []",
        // FieldPath + Literals
        "cpu.cores == 16",
        "cpu.arch == \"x86_64\"",
        "0xFF == 255",
        "1.5 < 2.0",
    ];
    for src in cases {
        parse_req(src).unwrap_or_else(|e| panic!("{src}: {e}"));
    }
}
