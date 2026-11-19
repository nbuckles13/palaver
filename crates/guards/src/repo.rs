//! Reading the real repository: source files and `cargo metadata`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The workspace root (two levels above this crate).
pub fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Every file with one of `extensions` under `crates/` and `apps/`, keyed by path relative to
/// the workspace root with forward slashes. Build output is skipped.
pub fn files(extensions: &[&str]) -> Result<BTreeMap<String, String>, String> {
    let root = root();
    let mut out = BTreeMap::new();
    for top in ["crates", "apps"] {
        walk(&root, &root.join(top), extensions, &mut out)?;
    }
    Ok(out)
}

fn walk(
    root: &Path,
    dir: &Path,
    extensions: &[&str],
    out: &mut BTreeMap<String, String>,
) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for entry in entries {
        let path = entry.map_err(|e| e.to_string())?.path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if path.is_dir() {
            if name != "target" {
                walk(root, &path, extensions, out)?;
            }
        } else if extensions
            .iter()
            .any(|x| name == *x || name.ends_with(&format!(".{x}")))
        {
            let rel = path.strip_prefix(root).map_err(|e| e.to_string())?;
            let key = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            let text =
                std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            out.insert(key, text);
        }
    }
    Ok(())
}

/// `cargo metadata` for the workspace, as JSON text.
///
/// Its time is cargo's, not ours, and it once took over five seconds on a CI runner with
/// nothing to show why. So this leaves a trail on the test's standard error, which the test
/// runner shows if the test fails or is killed for taking too long:
///
/// - a line before cargo starts and one after, with how long it took;
/// - cargo's own messages as they happen, not collected at the end: that is where
///   "Blocking waiting for file lock on package cache" appears if another cargo holds it;
/// - cargo's verbose output, which says when it updates an index or downloads anything.
pub fn cargo_metadata() -> Result<String, String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| String::from("cargo"));
    let started = std::time::Instant::now();
    eprintln!("guards: running `{cargo} metadata --locked -v`");
    let out = std::process::Command::new(cargo)
        .args(["metadata", "--format-version", "1", "--locked", "-v"])
        .current_dir(root())
        .stderr(std::process::Stdio::inherit())
        .output()
        .map_err(|e| format!("running cargo metadata: {e}"))?;
    eprintln!(
        "guards: cargo metadata finished in {} ms with {}, {} bytes of output",
        started.elapsed().as_millis(),
        out.status,
        out.stdout.len()
    );
    if !out.status.success() {
        return Err(String::from(
            "cargo metadata failed; its messages are above",
        ));
    }
    String::from_utf8(out.stdout).map_err(|e| e.to_string())
}
