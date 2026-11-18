//! Test-only features: code that must never be compiled into anything that ships.
//!
//! `palaver-proto`'s `fake-crypto` feature compiles a stand-in signer that anyone can forge.
//! Every test would still pass with it switched on in a real build, because tests use it too,
//! so only a check on the dependency graph can hold the line. The rules:
//!
//! 1. The feature exists, and is not a default feature.
//! 2. Only the crates on its allow list may turn it on in a normal dependency, or forward it
//!    through a feature of their own. Dev-dependencies are free to: they never ship.
//! 3. Nothing that ships may depend, directly or transitively, on a crate on the allow list.
//!    Cargo unifies features across a build, so depending on such a crate would switch the
//!    feature on for the whole binary.

use crate::layering::Graph;
use std::collections::{BTreeMap, BTreeSet};

/// A feature that only test infrastructure may enable.
pub struct TestOnlyFeature {
    pub krate: &'static str,
    pub feature: &'static str,
    /// Crates that may enable it in a normal dependency.
    pub allowed: &'static [&'static str],
}

pub const TEST_ONLY_FEATURES: &[TestOnlyFeature] = &[TestOnlyFeature {
    krate: "palaver-proto",
    feature: "fake-crypto",
    allowed: &["palaver-sim"],
}];

/// Crates that end up in what users run.
pub const SHIPPING: &[&str] = &[
    "palaver-sdk",
    "palaver-net",
    "palaver-media",
    "palaver-bot",
    "palaver-harness",
];

/// One dependency declaration in a manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dependency {
    pub from: String,
    pub on: String,
    /// False for dev-dependencies.
    pub ships: bool,
    pub features: Vec<String>,
}

/// What the manifests of the workspace say about features.
#[derive(Debug, Clone, Default)]
pub struct Manifests {
    /// Package -> its `[features]` table.
    pub features: BTreeMap<String, BTreeMap<String, Vec<String>>>,
    pub dependencies: Vec<Dependency>,
}

/// Read the manifests out of `cargo metadata` JSON.
pub fn manifests_from_metadata(json: &str) -> Result<Manifests, String> {
    let v: serde_json::Value = serde_json::from_str(json).map_err(|e| e.to_string())?;
    let strings = |value: &serde_json::Value| -> Vec<String> {
        value
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|x| x.as_str().map(str::to_owned))
            .collect()
    };
    let mut out = Manifests::default();
    for package in v["packages"].as_array().ok_or("no packages")? {
        let name = package["name"].as_str().ok_or("package without a name")?;
        let table = package["features"]
            .as_object()
            .ok_or("package without features")?;
        out.features.insert(
            name.to_owned(),
            table.iter().map(|(k, v)| (k.clone(), strings(v))).collect(),
        );
        for dep in package["dependencies"]
            .as_array()
            .ok_or("package without dependencies")?
        {
            out.dependencies.push(Dependency {
                from: name.to_owned(),
                on: dep["name"]
                    .as_str()
                    .ok_or("dependency without a name")?
                    .to_owned(),
                ships: dep["kind"].as_str() != Some("dev"),
                features: strings(&dep["features"]),
            });
        }
    }
    Ok(out)
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

/// Violations of the rules above. `graph` holds normal dependencies only.
pub fn violations(rules: &[TestOnlyFeature], manifests: &Manifests, graph: &Graph) -> Vec<String> {
    let mut out = vec![];
    for rule in rules {
        let (krate, feature) = (rule.krate, rule.feature);
        let Some(table) = manifests.features.get(krate) else {
            out.push(format!(
                "{krate}: not found in the workspace (renamed? update the guard)"
            ));
            continue;
        };
        if !table.contains_key(feature) {
            out.push(format!(
                "{krate} has no feature {feature} (renamed? update the guard)"
            ));
            continue;
        }
        if table
            .get("default")
            .is_some_and(|d| d.iter().any(|f| f == feature))
        {
            out.push(format!("{krate}: {feature} must not be a default feature"));
        }
        for dep in &manifests.dependencies {
            let enables = dep.on == krate && dep.features.iter().any(|f| f == feature);
            if enables && dep.ships && !rule.allowed.contains(&dep.from.as_str()) {
                out.push(format!(
                    "{} enables {krate}/{feature} in a normal dependency",
                    dep.from
                ));
            }
        }
        // A crate's own feature can forward it: `x = ["palaver-proto/fake-crypto"]`.
        let forwards = [format!("{krate}/{feature}"), format!("{krate}?/{feature}")];
        for (package, features) in &manifests.features {
            let forwarded = features.values().flatten().any(|f| forwards.contains(f));
            if forwarded && !rule.allowed.contains(&package.as_str()) {
                out.push(format!(
                    "{package} forwards {krate}/{feature} through a feature of its own"
                ));
            }
        }
        for shipping in SHIPPING {
            if !graph.contains_key(*shipping) {
                out.push(format!(
                    "{shipping}: not found in the workspace (renamed? update the guard)"
                ));
                continue;
            }
            let reach = reachable(graph, shipping);
            for allowed in rule.allowed {
                if reach.contains(*allowed) || shipping == allowed {
                    out.push(format!("{shipping} ships but depends on {allowed}, which may enable {krate}/{feature}"));
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{layering, repo};

    const RULES: &[TestOnlyFeature] = &[TestOnlyFeature {
        krate: "core",
        feature: "fake",
        allowed: &["simulator"],
    }];

    fn dep(from: &str, on: &str, ships: bool, features: &[&str]) -> Dependency {
        Dependency {
            from: from.into(),
            on: on.into(),
            ships,
            features: features.iter().map(|f| (*f).into()).collect(),
        }
    }

    /// `core` declares `fake`; the simulator enables it; the shipping crates depend on `core`
    /// without it.
    fn clean() -> (Manifests, Graph) {
        let mut m = Manifests::default();
        m.features
            .insert("core".into(), [("fake".to_owned(), vec![])].into());
        m.features.insert("simulator".into(), BTreeMap::new());
        m.dependencies
            .push(dep("simulator", "core", true, &["fake"]));
        let mut g = Graph::new();
        for shipping in SHIPPING {
            m.features.insert((*shipping).into(), BTreeMap::new());
            m.dependencies.push(dep(shipping, "core", true, &[]));
            g.entry((*shipping).into())
                .or_default()
                .insert("core".into());
        }
        g.entry("simulator".into())
            .or_default()
            .insert("core".into());
        g.entry("core".into()).or_default();
        (m, g)
    }

    // ---- self-tests: each rule fails on a seeded violation ----
    #[test]
    fn the_clean_fixture_has_no_violations() {
        let (m, g) = clean();
        assert_eq!(violations(RULES, &m, &g), Vec::<String>::new());
    }

    #[test]
    fn a_shipping_crate_enabling_the_feature_is_caught() {
        let (mut m, g) = clean();
        m.dependencies
            .push(dep("palaver-net", "core", true, &["fake"]));
        assert_eq!(
            violations(RULES, &m, &g),
            ["palaver-net enables core/fake in a normal dependency"]
        );
    }

    #[test]
    fn enabling_it_in_a_dev_dependency_is_allowed() {
        let (mut m, g) = clean();
        m.dependencies
            .push(dep("palaver-net", "core", false, &["fake"]));
        assert_eq!(violations(RULES, &m, &g), Vec::<String>::new());
    }

    #[test]
    fn forwarding_it_through_another_feature_is_caught() {
        let (mut m, g) = clean();
        m.features.insert(
            "palaver-sdk".into(),
            [("testing".to_owned(), vec!["core/fake".to_owned()])].into(),
        );
        assert_eq!(
            violations(RULES, &m, &g),
            ["palaver-sdk forwards core/fake through a feature of its own"]
        );
    }

    #[test]
    fn making_it_a_default_feature_is_caught() {
        let (mut m, g) = clean();
        m.features.insert(
            "core".into(),
            [
                ("fake".to_owned(), vec![]),
                ("default".to_owned(), vec!["fake".to_owned()]),
            ]
            .into(),
        );
        assert_eq!(
            violations(RULES, &m, &g),
            ["core: fake must not be a default feature"]
        );
    }

    #[test]
    fn a_shipping_crate_reaching_the_simulator_is_caught_even_transitively() {
        let (m, mut g) = clean();
        g.entry("palaver-bot".into())
            .or_default()
            .insert("helper".into());
        g.entry("helper".into())
            .or_default()
            .insert("simulator".into());
        assert_eq!(
            violations(RULES, &m, &g),
            ["palaver-bot ships but depends on simulator, which may enable core/fake"]
        );
    }

    #[test]
    fn a_missing_crate_or_feature_is_a_violation_not_a_pass() {
        let (mut m, g) = clean();
        m.features.insert("core".into(), BTreeMap::new());
        assert_eq!(
            violations(RULES, &m, &g),
            ["core has no feature fake (renamed? update the guard)"]
        );
        m.features.remove("core");
        assert_eq!(
            violations(RULES, &m, &g),
            ["core: not found in the workspace (renamed? update the guard)"]
        );
    }

    #[test]
    fn dev_dependencies_are_told_apart_in_cargo_metadata() {
        let json = r#"{"packages":[{"name":"a","features":{"x":["b/fake"]},"dependencies":[
            {"name":"b","kind":null,"features":["fake"]},{"name":"c","kind":"dev","features":[]}]}]}"#;
        let m = manifests_from_metadata(json).unwrap();
        assert_eq!(
            m.dependencies,
            [dep("a", "b", true, &["fake"]), dep("a", "c", false, &[])]
        );
        assert_eq!(m.features["a"]["x"], ["b/fake"]);
    }

    // ---- the real repository ----
    #[test]
    fn nothing_that_ships_can_enable_a_test_only_feature() {
        let json = repo::cargo_metadata().unwrap();
        let manifests = manifests_from_metadata(&json).unwrap();
        let graph = layering::graph_from_metadata(&json).unwrap();
        let ours = manifests
            .features
            .keys()
            .filter(|k| k.starts_with("palaver-"))
            .count();
        assert!(
            ours >= 10,
            "only {ours} workspace crates seen: nothing to check"
        );
        assert!(!TEST_ONLY_FEATURES.is_empty());
        assert_eq!(
            violations(TEST_ONLY_FEATURES, &manifests, &graph),
            Vec::<String>::new()
        );
    }
}
