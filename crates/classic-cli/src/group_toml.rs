//! TOML schema -> classic_place::PlacementGroup.
//!
//! Schema (per plan-07 §Example group.toml):
//!
//! ```toml
//! strategy = "pack"   # or "spread", case-insensitive
//!
//! [[member]]
//! label = "trainer"
//! requires = "any(gpu, gpu.vram_mb >= 80000)"
//! argv = ["python", "train.py"]
//! env = [["RANK", "0"]]
//! ```
//!
//! `env` entries are `[KEY, VALUE]` pairs; `argv` is `Vec<String>`.
//! Both default to empty if omitted. The plan-03 predicate is parsed
//! eagerly so the CLI fails before any network I/O.
//!
//! Member labels must be unique (FR-8). Duplicate labels are
//! reported with the offending label name.

use std::path::Path;

use classic_place::{parse_req, GroupMember, GroupStrategy, ParseError, PlacementGroup};
use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum GroupTomlError {
    #[error("file not found: {0}")]
    NotFound(String),
    #[error("io: {0}")]
    Io(String),
    #[error("toml: {0}")]
    Toml(String),
    #[error("strategy '{0}' is not one of: pack, spread")]
    UnknownStrategy(String),
    #[error("predicate for member '{label}' is invalid: {err}")]
    Predicate { label: String, err: ParseError },
    #[error("duplicate member label: {0}")]
    DuplicateLabel(String),
    #[error("group has no members")]
    Empty,
}

#[derive(Deserialize)]
struct RawGroup {
    strategy: String,
    #[serde(default)]
    member: Vec<RawMember>,
}

#[derive(Deserialize)]
struct RawMember {
    label: String,
    requires: String,
    #[serde(default)]
    argv: Vec<String>,
    #[serde(default)]
    env: Vec<Vec<String>>,
}

/// Read and parse a group.toml file into a fully-validated
/// `PlacementGroup`. Parses the plan-03 predicate per member and
/// validates label uniqueness before returning.
pub fn parse_group_toml(path: &Path) -> Result<PlacementGroup, GroupTomlError> {
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(GroupTomlError::NotFound(path.display().to_string()));
        }
        Err(e) => return Err(GroupTomlError::Io(e.to_string())),
    };
    parse_group_toml_str(&src)
}

/// Same as `parse_group_toml` but takes the TOML source string
/// directly. Exposed for tests that don't want a tempfile dance.
pub fn parse_group_toml_str(src: &str) -> Result<PlacementGroup, GroupTomlError> {
    let raw: RawGroup = toml::from_str(src).map_err(|e| GroupTomlError::Toml(e.to_string()))?;
    let strategy = match raw.strategy.to_ascii_lowercase().as_str() {
        "pack" => GroupStrategy::Pack,
        "spread" => GroupStrategy::Spread,
        other => return Err(GroupTomlError::UnknownStrategy(other.into())),
    };
    if raw.member.is_empty() {
        return Err(GroupTomlError::Empty);
    }
    let mut seen: std::collections::HashSet<String> = Default::default();
    let mut members: Vec<GroupMember> = Vec::with_capacity(raw.member.len());
    for m in raw.member {
        if !seen.insert(m.label.clone()) {
            return Err(GroupTomlError::DuplicateLabel(m.label));
        }
        let req = parse_req(&m.requires).map_err(|err| GroupTomlError::Predicate {
            label: m.label.clone(),
            err,
        })?;
        let env: Vec<(String, String)> = m
            .env
            .into_iter()
            .filter_map(|pair| {
                let mut it = pair.into_iter();
                match (it.next(), it.next()) {
                    (Some(k), Some(v)) => Some((k, v)),
                    _ => None,
                }
            })
            .collect();
        members.push(GroupMember {
            label: m.label,
            req,
            requires_src: m.requires,
            argv: m.argv,
            env,
        });
    }
    Ok(PlacementGroup { strategy, members })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pack_example() -> &'static str {
        r#"
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
"#
    }

    fn spread_example() -> &'static str {
        r#"
strategy = "spread"

[[member]]
label = "a"
requires = "any(gpu, gpu.vendor == 0x10de)"
argv = ["a.sh"]

[[member]]
label = "b"
requires = "any(gpu, gpu.vendor == 0x10de)"
argv = ["b.sh"]
"#
    }

    #[test]
    fn parse_pack_example() {
        let g = parse_group_toml_str(pack_example()).unwrap();
        assert_eq!(g.strategy, GroupStrategy::Pack);
        assert_eq!(g.members.len(), 2);
        assert_eq!(g.members[0].label, "trainer");
        assert_eq!(g.members[1].label, "worker");
        assert_eq!(g.members[0].argv, vec!["python", "train.py"]);
        assert_eq!(g.members[0].env, vec![("RANK".into(), "0".into())]);
        // requires_src preserved verbatim for downstream wire frames.
        assert_eq!(
            g.members[0].requires_src,
            "any(gpu, gpu.vram_mb >= 80000)"
        );
    }

    #[test]
    fn parse_spread_example() {
        let g = parse_group_toml_str(spread_example()).unwrap();
        assert_eq!(g.strategy, GroupStrategy::Spread);
        assert_eq!(g.members.len(), 2);
    }

    #[test]
    fn strategy_case_insensitive() {
        let src = r#"
strategy = "PACK"
[[member]]
label = "a"
requires = "true"
"#;
        let g = parse_group_toml_str(src).unwrap();
        assert_eq!(g.strategy, GroupStrategy::Pack);
    }

    #[test]
    fn duplicate_label_rejected() {
        let src = r#"
strategy = "pack"
[[member]]
label = "a"
requires = "true"
[[member]]
label = "a"
requires = "true"
"#;
        match parse_group_toml_str(src).unwrap_err() {
            GroupTomlError::DuplicateLabel(l) => assert_eq!(l, "a"),
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
            other => panic!("expected Toml parse error, got {other:?}"),
        }
    }

    #[test]
    fn unknown_strategy() {
        let src = r#"
strategy = "scatter"
[[member]]
label = "a"
requires = "true"
"#;
        match parse_group_toml_str(src).unwrap_err() {
            GroupTomlError::UnknownStrategy(s) => assert_eq!(s, "scatter"),
            other => panic!("expected UnknownStrategy, got {other:?}"),
        }
    }

    #[test]
    fn bad_predicate_string() {
        let src = r#"
strategy = "pack"
[[member]]
label = "a"
requires = "this is not valid 12345"
"#;
        match parse_group_toml_str(src).unwrap_err() {
            GroupTomlError::Predicate { label, .. } => assert_eq!(label, "a"),
            other => panic!("expected Predicate, got {other:?}"),
        }
    }

    #[test]
    fn empty_group_rejected() {
        let src = r#"strategy = "pack""#;
        match parse_group_toml_str(src).unwrap_err() {
            GroupTomlError::Empty => {}
            other => panic!("expected Empty, got {other:?}"),
        }
    }
}
