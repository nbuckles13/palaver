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
pub fn cargo_metadata() -> Result<String, String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| String::from("cargo"));
    let out = std::process::Command::new(cargo)
        .args(["metadata", "--format-version", "1", "--locked"])
        .current_dir(root())
        .output()
        .map_err(|e| format!("running cargo metadata: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    String::from_utf8(out.stdout).map_err(|e| e.to_string())
}
