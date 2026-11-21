//! Layering: what the sans-IO crates may depend on, and that their lint configuration is in
//! place.

use std::collections::{BTreeMap, BTreeSet};

/// Crates that hold pure decision logic: no runtime, sockets, clock or ambient randomness.
pub const SANS_IO: &[&str] = &[
    "palaver-moq-wire",
    "palaver-proto",
    "palaver-khef-core",
    "palaver-topology",
    "palaver-control",
    "palaver-node",
    "palaver-sim",
];

/// Crates a sans-IO crate must not reach, directly or transitively.
pub const BANNED_FOR_SANS_IO: &[&str] = &[
    "tokio",
    "async-std",
    "smol",
    "mio",
    "socket2",
    "quinn",
    "iroh",
    "palaver-net",
    "palaver-media",
    "palaver-sound",
    "cpal",
    "palaver-sdk",
];

/// Extra edges that must not exist: (from, to, reason).
pub const FORBIDDEN_EDGES: &[(&str, &str, &str)] = &[(
    "palaver-khef-core",
    "palaver-media",
    "khefs are media-blind",
)];

/// Package name -> names of its normal (non-dev, non-build) dependencies.
pub type Graph = BTreeMap<String, BTreeSet<String>>;

/// Build the graph from `cargo metadata` JSON.
///
/// # Errors
///
/// Fails if `json` is not JSON, or lacks the package list or resolve graph that
/// `cargo metadata` writes.
pub fn graph_from_metadata(json: &str) -> Result<Graph, String> {
    let v: serde_json::Value = serde_json::from_str(json).map_err(|e| e.to_string())?;
    let mut names = BTreeMap::new();
    for p in v["packages"].as_array().ok_or("no packages")? {
        let (Some(id), Some(name)) = (p["id"].as_str(), p["name"].as_str()) else {
            return Err("bad package".into());
        };
        names.insert(id.to_owned(), name.to_owned());
    }
    let mut graph = Graph::new();
    for node in v["resolve"]["nodes"].as_array().ok_or("no resolve graph")? {
        let from = node["id"]
            .as_str()
            .and_then(|id| names.get(id))
            .ok_or("unknown node")?;
        let deps = graph.entry(from.clone()).or_default();
        for dep in node["deps"].as_array().ok_or("no deps")? {
            let normal = dep["dep_kinds"]
                .as_array()
                .is_some_and(|k| k.iter().any(|x| x["kind"].is_null()));
            if let (true, Some(to)) = (normal, dep["pkg"].as_str().and_then(|id| names.get(id))) {
                deps.insert(to.clone());
            }
        }
    }
    Ok(graph)
}

fn reachable(graph: &Graph, from: &str) -> BTreeSet<String> {
    let mut seen = BTreeSet::new();
    let mut todo = vec![from.to_owned()];
    while let Some(n) = todo.pop() {
        for d in graph.get(&n).into_iter().flatten() {
            if seen.insert(d.clone()) {
                todo.push(d.clone());
            }
        }
    }
    seen
}

/// Layering violations in `graph`. Fails (as a violation) if an expected crate is missing, so
/// renaming a crate cannot silently switch the check off.
#[must_use]
pub fn violations(graph: &Graph) -> Vec<String> {
    let mut out = vec![];
    for krate in SANS_IO {
        if !graph.contains_key(*krate) {
            out.push(format!(
                "{krate}: not found in the workspace (renamed? update the guard)"
            ));
            continue;
        }
        let reach = reachable(graph, krate);
        for banned in BANNED_FOR_SANS_IO {
            if reach.contains(*banned) {
                out.push(format!("{krate} is sans-IO but depends on {banned}"));
            }
        }
    }
    for (from, to, why) in FORBIDDEN_EDGES {
        if reachable(graph, from).contains(*to) {
            out.push(format!("{from} must not depend on {to}: {why}"));
        }
    }
    out
}

/// Bans every sans-IO crate's `clippy.toml` must contain.
pub const REQUIRED_BANS: &[&str] = &[
    "std::time::Instant::now",
    "std::time::SystemTime::now",
    "std::thread::sleep",
    "std::thread::spawn",
    "std::collections::HashMap",
    "std::collections::HashSet",
    "std::net::UdpSocket",
    "std::net::TcpStream",
];

/// Which required bans are missing from one `clippy.toml`.
#[must_use]
pub fn missing_bans(clippy_toml: &str) -> Vec<&'static str> {
    REQUIRED_BANS
        .iter()
        .copied()
        .filter(|b| !clippy_toml.contains(&format!("\"{b}\"")))
        .collect()
}

/// Whether a crate manifest inherits the workspace lints.
pub fn inherits_workspace_lints(cargo_toml: &str) -> bool {
    let mut in_lints = false;
    for line in cargo_toml.lines().map(str::trim) {
        if line.starts_with('[') {
            in_lints = line == "[lints]";
        } else if in_lints && line.replace(' ', "") == "workspace=true" {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo;

    fn fixture(edges: &[(&str, &str)]) -> Graph {
        let mut g = Graph::new();
        for k in SANS_IO {
            g.entry((*k).to_owned()).or_default();
        }
        for (a, b) in edges {
            g.entry((*a).to_owned())
                .or_default()
                .insert((*b).to_owned());
            g.entry((*b).to_owned()).or_default();
        }
        g
    }

    // ---- self-tests: each rule fails on a seeded violation ----
    #[test]
    fn clean_fixture_has_no_violations() {
        assert_eq!(
            violations(&fixture(&[("palaver-khef-core", "palaver-proto")])),
            Vec::<String>::new()
        );
    }

    #[test]
    fn transitive_runtime_dependency_is_caught() {
        let v = violations(&fixture(&[
            ("palaver-khef-core", "helper"),
            ("helper", "tokio"),
        ]));
        assert_eq!(v, ["palaver-khef-core is sans-IO but depends on tokio"]);
    }

    #[test]
    fn khef_core_reaching_media_is_caught() {
        let v = violations(&fixture(&[("palaver-khef-core", "palaver-media")]));
        assert!(v.iter().any(|m| m.contains("media-blind")), "{v:?}");
    }

    #[test]
    fn a_missing_sans_io_crate_is_a_violation_not_a_pass() {
        let mut g = fixture(&[]);
        g.remove("palaver-sim");
        assert!(
            violations(&g)
                .iter()
                .any(|m| m.contains("palaver-sim: not found"))
        );
    }

    #[test]
    fn dev_dependencies_do_not_count() {
        let json = r#"{"packages":[{"id":"a","name":"palaver-proto"},{"id":"t","name":"tokio"}],
            "resolve":{"nodes":[{"id":"a","deps":[{"pkg":"t","dep_kinds":[{"kind":"dev"}]}]},{"id":"t","deps":[]}]}}"#;
        let g = graph_from_metadata(json).unwrap();
        assert!(g["palaver-proto"].is_empty());
    }

    #[test]
    fn a_clippy_config_without_the_hash_map_ban_is_caught() {
        let good = REQUIRED_BANS
            .iter()
            .map(|b| format!("{{ path = \"{b}\" }}\n"))
            .collect::<Vec<_>>()
            .concat();
        assert!(missing_bans(&good).is_empty());
        let bad = good.replace("std::collections::HashMap", "std::collections::BTreeMap");
        assert_eq!(missing_bans(&bad), ["std::collections::HashMap"]);
    }

    #[test]
    fn a_manifest_without_workspace_lints_is_caught() {
        assert!(inherits_workspace_lints(
            "[package]\nname = \"x\"\n\n[lints]\nworkspace = true\n"
        ));
        assert!(!inherits_workspace_lints("[package]\nname = \"x\"\n"));
        assert!(!inherits_workspace_lints(
            "[package]\nname = \"x\"\n[lints.clippy]\nunwrap_used = \"allow\"\n"
        ));
    }

    // ---- the real repository ----
    // (The dependency graph is checked in tests/workspace.rs, from one `cargo metadata`.)

    #[test]
    fn sans_io_crates_carry_the_required_lint_bans() {
        let configs = repo::files(&["clippy.toml"]).unwrap();
        for krate in SANS_IO {
            let dir = krate.trim_start_matches("palaver-");
            let path = format!("crates/{dir}/clippy.toml");
            let text = configs
                .get(&path)
                .unwrap_or_else(|| panic!("{path} is missing"));
            assert_eq!(missing_bans(text), Vec::<&str>::new(), "{path}");
        }
    }

    #[test]
    fn every_crate_inherits_the_workspace_lints() {
        let manifests = repo::files(&["Cargo.toml"]).unwrap();
        assert!(
            manifests.len() >= 10,
            "only {} manifests seen: nothing to check",
            manifests.len()
        );
        let bad: Vec<&String> = manifests
            .iter()
            .filter(|(_, t)| !inherits_workspace_lints(t))
            .map(|(p, _)| p)
            .collect();
        assert_eq!(bad, Vec::<&String>::new());
    }
}
