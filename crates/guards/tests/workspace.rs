//! The guards on the real dependency graph, from one run of `cargo metadata`.
//!
//! Both guards read the same graph, so they share one test and one cargo process: two would
//! cost twice as much and could wait on each other's lock.
//!
//! CI runs this binary on Linux only (`.github/workflows/ci.yml`): `cargo metadata` resolves
//! dependencies for every platform wherever it runs, so the answer is the same on all of
//! them. Locally it runs wherever the tests are run.
#![allow(clippy::unwrap_used)]

use palaver_guards::test_only::TEST_ONLY_FEATURES;
use palaver_guards::{layering, repo, test_only};

#[test]
fn the_workspace_respects_layering_and_nothing_that_ships_enables_a_test_only_feature() {
    let json = repo::cargo_metadata().unwrap();
    let graph = layering::graph_from_metadata(&json).unwrap();
    let manifests = test_only::manifests_from_metadata(&json).unwrap();

    // No vacuous pass: both views must have seen the workspace.
    let in_graph = graph.keys().filter(|k| k.starts_with("palaver-")).count();
    let in_manifests = manifests
        .features
        .keys()
        .filter(|k| k.starts_with("palaver-"))
        .count();
    assert!(
        in_graph >= 10 && in_manifests >= 10,
        "only {in_graph} and {in_manifests} workspace crates seen: nothing to check"
    );
    assert!(!TEST_ONLY_FEATURES.is_empty());

    // Sans-IO crates reach no runtime, socket or shipping crate.
    assert_eq!(layering::violations(&graph), Vec::<String>::new());
    // The stand-in signer cannot be compiled into anything that ships.
    assert_eq!(
        test_only::violations(TEST_ONLY_FEATURES, &manifests, &graph),
        Vec::<String>::new()
    );
}
