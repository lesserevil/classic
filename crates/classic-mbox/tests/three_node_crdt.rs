//! Plan-05 integration test: 3-node service-directory CRDT convergence.
//! Three "nodes" (simulated by direct calls into the directory's apply
//! functions, since this is a logic test) each declare some services;
//! every node's directory observes the union eventually.

mod common;

use classic_mbox::{
    apply_remote_ad, apply_remote_forget, service_lookup, service_lookup_one,
};

#[test]
fn three_nodes_converge_on_shared_directory() {
    let _g = common::IT_MUTEX.lock().unwrap();
    classic_mbox::directory::test_clear();

    // Node 1 declares "registry" at its NetId.
    apply_remote_ad("registry", common::netid(1, 5), 10);
    // Node 2 declares "registry" at its own NetId (multi-endpoint!).
    apply_remote_ad("registry", common::netid(2, 5), 11);
    // Node 3 declares "scheduler".
    apply_remote_ad("scheduler", common::netid(3, 7), 12);

    let mut reg = service_lookup("registry");
    reg.sort_by(|a, b| a.node.0.cmp(&b.node.0));
    assert_eq!(reg, vec![common::netid(1, 5), common::netid(2, 5)]);

    assert_eq!(service_lookup("scheduler"), vec![common::netid(3, 7)]);

    // round-robin returns each endpoint in turn for the multi-endpoint name.
    let a = service_lookup_one("registry").unwrap();
    let b = service_lookup_one("registry").unwrap();
    assert_ne!(a, b);
}

#[test]
fn last_writer_wins_on_same_net_id() {
    let _g = common::IT_MUTEX.lock().unwrap();
    classic_mbox::directory::test_clear();

    apply_remote_ad("svc", common::netid(1, 1), 5);
    // Older lamport drops.
    apply_remote_ad("svc", common::netid(1, 1), 3);
    // Newer lamport replaces.
    apply_remote_ad("svc", common::netid(1, 1), 9);
    // Forget at lamport 10 wins over the live state.
    apply_remote_forget("svc", common::netid(1, 1), 10);

    assert!(service_lookup("svc").is_empty());

    // A later-arriving ad with a strictly-greater lamport revives it.
    apply_remote_ad("svc", common::netid(1, 1), 11);
    assert_eq!(service_lookup("svc"), vec![common::netid(1, 1)]);
}
