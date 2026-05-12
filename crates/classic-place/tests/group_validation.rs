//! End-to-end validation: empty-group rejection, duplicate-label
//! rejection, and confirmation that validation runs *before* strategy
//! dispatch (so an empty PACK reports Empty, not PackInfeasible(0)).

use classic_place::{
    parse_req, place_group, GroupMember, GroupPlaceError, GroupStrategy, PlacementGroup,
};

fn member(label: &str, req_src: &str) -> GroupMember {
    GroupMember {
        label: label.into(),
        req: parse_req(req_src).expect("test predicate must parse"),
        argv: vec![],
        env: vec![],
    }
}

#[test]
fn empty_group_returns_empty_error() {
    let g = PlacementGroup {
        strategy: GroupStrategy::Spread,
        members: vec![],
    };
    assert_eq!(place_group(&g, &[]).unwrap_err(), GroupPlaceError::Empty);
}

#[test]
fn duplicate_label_rejected() {
    let g = PlacementGroup {
        strategy: GroupStrategy::Spread,
        members: vec![
            member("trainer", "true"),
            member("trainer", "true"),
        ],
    };
    assert_eq!(
        place_group(&g, &[]).unwrap_err(),
        GroupPlaceError::DuplicateLabel("trainer".into()),
    );
}

#[test]
fn validation_runs_before_strategy() {
    // Empty + Pack must report Empty, not PackInfeasible(0). The
    // validator is unconditionally first.
    let g = PlacementGroup {
        strategy: GroupStrategy::Pack,
        members: vec![],
    };
    assert_eq!(place_group(&g, &[]).unwrap_err(), GroupPlaceError::Empty);
}
