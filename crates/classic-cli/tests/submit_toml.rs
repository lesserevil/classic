//! TOML schema integration tests. The submit subcommand's end-to-end
//! pathway (binary execution + UDS round-trip to a real daemon) is
//! exercised by classic-ibi; these tests cover the parts we own
//! locally: schema → PlacementGroup, error surfaces.

use classic_cli::group_toml::{parse_group_toml_str, GroupTomlError};
use classic_place::GroupStrategy;

#[test]
fn parse_pack_example() {
    let src = r#"
strategy = "pack"

[[member]]
label = "trainer"
requires = "any(gpu, gpu.vram_mb >= 80000)"
argv = ["python", "train.py"]
env = [["RANK", "0"]]

[[member]]
label = "worker"
requires = "true"
argv = ["python", "worker.py"]
"#;
    let g = parse_group_toml_str(src).unwrap();
    assert_eq!(g.strategy, GroupStrategy::Pack);
    assert_eq!(g.members.len(), 2);
    assert_eq!(g.members[0].label, "trainer");
    assert_eq!(g.members[1].label, "worker");
}

#[test]
fn parse_spread_example() {
    let src = r#"
strategy = "spread"

[[member]]
label = "a"
requires = "true"
argv = ["a.sh"]

[[member]]
label = "b"
requires = "true"
argv = ["b.sh"]
"#;
    let g = parse_group_toml_str(src).unwrap();
    assert_eq!(g.strategy, GroupStrategy::Spread);
    assert_eq!(g.members.len(), 2);
}

#[test]
fn duplicate_label_rejected() {
    let src = r#"
strategy = "pack"
[[member]]
label = "x"
requires = "true"
[[member]]
label = "x"
requires = "true"
"#;
    match parse_group_toml_str(src).unwrap_err() {
        GroupTomlError::DuplicateLabel(s) => assert_eq!(s, "x"),
        other => panic!("expected DuplicateLabel, got {other:?}"),
    }
}

#[test]
fn missing_strategy_field() {
    let src = r#"
[[member]]
label = "a"
requires = "true"
"#;
    match parse_group_toml_str(src).unwrap_err() {
        GroupTomlError::Toml(_) => {}
        other => panic!("expected Toml error, got {other:?}"),
    }
}

#[test]
fn bad_predicate_string() {
    let src = r#"
strategy = "pack"
[[member]]
label = "x"
requires = "not a valid predicate ###"
"#;
    match parse_group_toml_str(src).unwrap_err() {
        GroupTomlError::Predicate { label, .. } => assert_eq!(label, "x"),
        other => panic!("expected Predicate, got {other:?}"),
    }
}
