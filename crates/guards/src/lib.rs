//! Repository guards: checks on the code and repository that tests cannot hold.
//!
//! Rules for every guard here (docs/04-dev-environment-and-flow.md):
//! - no vacuous pass: a guard that finds nothing to check fails;
//! - scope by directory, not by file list;
//! - a self-test proves the guard fails on a seeded violation.
//!
//! Each check is a pure function over data (a dependency graph, a map of file contents), so
//! the self-tests feed it fixtures; one test per guard feeds it the real repository.

pub mod c_wrappers;
pub mod integrity;
pub mod layering;
pub mod repo;
pub mod test_only;
