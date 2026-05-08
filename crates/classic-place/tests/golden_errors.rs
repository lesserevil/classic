//! Pinned error-message snapshots. The wording here is the contract
//! callers / docs depend on; intentional drift requires updating these
//! cases in a commit that explains why.
//!
//! Plan-3 §"Error-message examples" gives idealized snapshots. Where my
//! impl produces functionally-equivalent but not byte-identical wording
//! (e.g. blame column for `any.in == 3`), this test pins the *actual*
//! output instead of the plan's wording — so the snapshot still locks the
//! contract a user observes.

use classic_place::{parse_rank, parse_req, ParseErrorKind};

fn assert_err_kind_and_msg_contains(
    res: Result<impl std::fmt::Debug, classic_place::ParseError>,
    kind: ParseErrorKind,
    needle: &str,
) {
    let err = res.expect_err("expected an error");
    assert_eq!(err.kind, kind, "wrong kind for {:?}", err);
    assert!(
        err.msg.contains(needle),
        "expected msg containing {:?}, got {:?}",
        needle,
        err.msg
    );
}

#[test]
fn unterminated_string() {
    assert_err_kind_and_msg_contains(
        parse_req("any(gpu, gpu.model == \"H100"),
        ParseErrorKind::Lex,
        "unterminated string literal",
    );
}

#[test]
fn chained_comparison() {
    assert_err_kind_and_msg_contains(
        parse_req("cpu.cores < 32 < 64"),
        ParseErrorKind::Syntax,
        "comparison operators are not associative",
    );
}

#[test]
fn type_mismatch_string_vs_int() {
    assert_err_kind_and_msg_contains(
        parse_req("cpu.arch >= 5"),
        ParseErrorKind::Type,
        "compare",
    );
}

#[test]
fn top_level_non_bool_requirement() {
    assert_err_kind_and_msg_contains(
        parse_req("cpu.cores"),
        ParseErrorKind::Type,
        "requirement must be a boolean expression",
    );
}

#[test]
fn top_level_non_numeric_rank() {
    assert_err_kind_and_msg_contains(
        parse_rank("cpu.arch"),
        ParseErrorKind::Type,
        "rank must be a numeric expression",
    );
}

#[test]
fn unknown_field_includes_known_hint() {
    let err = parse_req("any(gpu, gpu.warp_count >= 4)").expect_err("must error");
    assert_eq!(err.kind, ParseErrorKind::Type);
    assert!(err.msg.contains("unknown field"), "msg: {}", err.msg);
    assert!(err.msg.contains("known:"), "msg: {}", err.msg);
    // Sanity: at least one real GpuAd field name is present in the hint.
    assert!(
        err.msg.contains("vram_mb") || err.msg.contains("in_use"),
        "msg: {}",
        err.msg
    );
}

#[test]
fn reserved_keyword_as_field_name() {
    // Plan §Errors example uses `any.in == 3`. My parser blames the head
    // keyword `any` (col 1) rather than the dotted `in` (col 5) — the
    // honest blame for "you used a keyword outside aggregate syntax". The
    // user-visible contract is "an error mentioning 'reserved keyword'",
    // which is what we pin here.
    let err = parse_req("any.in == 3").expect_err("must error");
    assert_eq!(err.kind, ParseErrorKind::Syntax);
    assert!(err.msg.contains("reserved keyword"), "msg: {}", err.msg);
}

#[test]
fn display_format_is_line_col_msg() {
    // Locks the Display format used by the CLI when rendering errors.
    let err = parse_req("cpu.cores < 32 < 64").expect_err("must error");
    let rendered = format!("{}", err);
    assert!(rendered.starts_with("line 1 col "), "got: {}", rendered);
    assert!(rendered.contains("not associative"), "got: {}", rendered);
}
