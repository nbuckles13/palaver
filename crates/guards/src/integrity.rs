//! Test integrity: the suite must not stay green while proving less.
//!
//! - A source file that no `mod` declaration reaches is never compiled, so its tests silently
//!   never run.
//! - A test disabled with a bare ignore attribute gives no reason a reviewer can see. Ignoring
//!   with a reason (`ignore = "..."`) is allowed: it is visible in the diff.

use std::collections::BTreeMap;

/// Whether `source` declares module `name` (`mod name;` with any visibility).
fn declares(source: &str, name: &str) -> bool {
    source.lines().map(str::trim).any(|line| {
        let rest = line.strip_prefix("pub").map_or(line, |r| {
            let r = r.trim_start();
            if r.starts_with('(') {
                r.split_once(')').map_or(r, |(_, after)| after)
            } else {
                r
            }
        });
        rest.trim_start()
            .strip_prefix("mod ")
            .is_some_and(|m| m.trim_end_matches(';').trim() == name)
    })
}

/// Files that are compiled without being declared by a parent: crate roots, binaries,
/// integration tests, benches, examples and build scripts.
fn is_root(path: &str) -> bool {
    let parts: Vec<&str> = path.split('/').collect();
    let n = parts.len();
    let (Some(file), Some(dir)) = (parts.last(), n.checked_sub(2).and_then(|i| parts.get(i)))
    else {
        return false;
    };
    matches!((*dir, *file), ("src", "lib.rs" | "main.rs"))
        || matches!(*dir, "bin" | "tests" | "benches" | "examples")
        || *file == "build.rs"
}

/// Rust files that nothing declares, so they are never compiled. `files` maps a path (forward
/// slashes) to its content.
pub fn orphans(files: &BTreeMap<String, String>) -> Vec<String> {
    let mut out = vec![];
    for path in files.keys().filter(|p| p.ends_with(".rs") && !is_root(p)) {
        let Some((dir, file)) = path.rsplit_once('/') else {
            continue;
        };
        // `foo/mod.rs` is module `foo` of the directory above; `foo/bar.rs` is `bar` of `foo`.
        let (module, home) = if file == "mod.rs" {
            match dir.rsplit_once('/') {
                Some((parent, name)) => (name, parent),
                None => continue,
            }
        } else {
            (file.trim_end_matches(".rs"), dir)
        };
        let candidates = [
            format!("{home}/mod.rs"),
            format!("{home}.rs"),
            format!("{home}/lib.rs"),
            format!("{home}/main.rs"),
        ];
        let mut declared = candidates
            .iter()
            .any(|c| files.get(c).is_some_and(|src| declares(src, module)));
        // Helper modules under `tests/` or `src/bin/` may be declared by any root beside them.
        if !declared && (home.ends_with("/tests") || home.ends_with("/bin")) {
            let prefix = format!("{home}/");
            declared = files.iter().any(|(p, src)| {
                p.strip_prefix(&prefix)
                    .is_some_and(|rest| !rest.contains('/'))
                    && declares(src, module)
            });
        }
        if !declared {
            out.push(path.clone());
        }
    }
    out
}

/// Lines that disable a test without a reason, as `path:line`.
pub fn bare_ignores(files: &BTreeMap<String, String>) -> Vec<String> {
    let mut out = vec![];
    for (path, text) in files {
        for (i, line) in text.lines().enumerate() {
            let line: String = line.split_whitespace().collect();
            if line.starts_with("#[ignore]") {
                out.push(format!("{path}:{}", i + 1));
            }
        }
    }
    out
}

/// How many test functions the files contain; used so the guard cannot pass on an empty scan.
pub fn test_count(files: &BTreeMap<String, String>) -> usize {
    files
        .values()
        .map(|t| t.lines().filter(|l| l.trim() == "#[test]").count())
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo;

    fn tree(files: &[(&str, &str)]) -> BTreeMap<String, String> {
        files
            .iter()
            .map(|(p, c)| ((*p).to_owned(), (*c).to_owned()))
            .collect()
    }

    // ---- self-tests ----
    #[test]
    fn declared_modules_are_not_orphans() {
        let t = tree(&[
            (
                "crates/x/src/lib.rs",
                "mod a;\npub mod b;\npub(crate) mod c;\n",
            ),
            ("crates/x/src/a.rs", "mod inner;\n"),
            ("crates/x/src/a/inner.rs", ""),
            ("crates/x/src/b/mod.rs", "mod deep;"),
            ("crates/x/src/b/deep.rs", ""),
            ("crates/x/src/c.rs", ""),
            ("crates/x/src/bin/tool.rs", ""),
            ("crates/x/tests/scenario.rs", "mod common;"),
            ("crates/x/tests/common/mod.rs", ""),
            ("crates/x/build.rs", ""),
        ]);
        assert_eq!(orphans(&t), Vec::<String>::new());
    }

    #[test]
    fn an_undeclared_file_is_caught() {
        let t = tree(&[
            ("crates/x/src/lib.rs", "mod a;\n"),
            ("crates/x/src/a.rs", ""),
            ("crates/x/src/forgotten.rs", ""),
        ]);
        assert_eq!(orphans(&t), ["crates/x/src/forgotten.rs"]);
    }

    #[test]
    fn an_undeclared_nested_file_and_test_helper_are_caught() {
        let t = tree(&[
            ("crates/x/src/lib.rs", "mod a;\n"),
            ("crates/x/src/a.rs", "// mod inner; (commented out)\n"),
            ("crates/x/src/a/inner.rs", ""),
            ("crates/x/tests/scenario.rs", ""),
            ("crates/x/tests/common/mod.rs", ""),
        ]);
        assert_eq!(
            orphans(&t),
            ["crates/x/src/a/inner.rs", "crates/x/tests/common/mod.rs"]
        );
    }

    #[test]
    fn a_bare_ignore_is_caught_and_a_reasoned_one_is_not() {
        let bare = format!("#[test]\n  #[{}]\nfn t() {{}}\n", "ignore");
        let reasoned = format!(
            "#[test]\n#[{} = \"needs a camera\"]\nfn t() {{}}\n",
            "ignore"
        );
        let t = tree(&[
            ("crates/x/src/lib.rs", &bare),
            ("crates/x/src/other.rs", &reasoned),
        ]);
        assert_eq!(bare_ignores(&t), ["crates/x/src/lib.rs:2"]);
    }

    // ---- the real repository ----
    #[test]
    fn repository_has_no_orphan_files_or_bare_ignores() {
        let files = repo::files(&["rs"]).unwrap();
        assert!(
            files.len() >= 15,
            "only {} Rust files seen: nothing to check",
            files.len()
        );
        assert!(
            test_count(&files) >= 10,
            "only {} tests seen: nothing to check",
            test_count(&files)
        );
        assert_eq!(orphans(&files), Vec::<String>::new());
        assert_eq!(bare_ignores(&files), Vec::<String>::new());
    }
}
