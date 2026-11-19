//! Unsafe code: only in the crates that wrap a C library.
//!
//! The workspace denies `unsafe_code`. A crate that wraps a C library has to allow it for
//! itself, so the lint is `deny` rather than `forbid`, and that leaves a door open: any
//! crate could write the same allow. This guard keeps the door to a list.
//!
//! - Outside the listed crates, no source line may use the keyword or allow the lint.
//! - A listed crate must actually contain some, or the list is stale.
//!
//! Inside a listed crate, clippy's `undocumented_unsafe_blocks` (denied workspace-wide)
//! requires a safety comment on every block.

use std::collections::BTreeMap;

/// Directories of the crates that wrap a C library, with a trailing slash.
pub const C_WRAPPERS: &[&str] = &["crates/opus/"];

// Built in pieces so that this file does not contain what it searches for.
const KEYWORD: &str = concat!("un", "safe");
const ALLOW: &str = concat!("allow(un", "safe_code)");

/// Whether a line of Rust uses the keyword or allows the lint. Comments do not count.
fn uses_it(line: &str) -> bool {
    let code = line.split("//").next().unwrap_or_default();
    if code.contains(ALLOW) {
        return true;
    }
    // The keyword as a word of its own: `x_unsafe` and `unsafety` are not it.
    let mut rest = code;
    while let Some(at) = rest.find(KEYWORD) {
        let before = rest[..at].chars().next_back();
        let after = rest[at + KEYWORD.len()..].chars().next();
        let is_word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
        if !is_word(before) && !is_word(after) {
            return true;
        }
        rest = &rest[at + KEYWORD.len()..];
    }
    false
}

/// `files` maps path to content. `wrappers` are the directories allowed to hold unsafe code.
pub fn violations(files: &BTreeMap<String, String>, wrappers: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    for (path, text) in files {
        if wrappers.iter().any(|dir| path.starts_with(dir)) {
            continue;
        }
        for (number, line) in text.lines().enumerate() {
            if uses_it(line) {
                out.push(format!(
                    "{path}:{}: {KEYWORD} code outside a C wrapper crate",
                    number + 1
                ));
            }
        }
    }
    for dir in wrappers {
        let has_some = files
            .iter()
            .any(|(path, text)| path.starts_with(dir) && text.lines().any(uses_it));
        if !has_some {
            out.push(format!(
                "{dir}: listed as a C wrapper but holds no {KEYWORD} code (removed? update the guard)"
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo;

    fn files(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(path, text)| ((*path).to_owned(), (*text).to_owned()))
            .collect()
    }

    fn block() -> String {
        format!("fn f() {{ {KEYWORD} {{ g() }} }}")
    }

    #[test]
    fn a_wrapper_with_unsafe_code_and_clean_crates_pass() {
        let block = block();
        let f = files(&[
            ("crates/opus/src/lib.rs", &block),
            ("crates/proto/src/lib.rs", "fn f() {}"),
        ]);
        assert_eq!(violations(&f, &["crates/opus/"]), Vec::<String>::new());
    }

    #[test]
    fn unsafe_code_outside_a_wrapper_is_caught_with_its_line() {
        let text = format!("fn a() {{}}\n{}\n", block());
        let f = files(&[
            ("crates/opus/src/lib.rs", &block()),
            ("crates/proto/src/lib.rs", &text),
        ]);
        let v = violations(&f, &["crates/opus/"]);
        assert_eq!(v.len(), 1, "{v:?}");
        assert!(v[0].starts_with("crates/proto/src/lib.rs:2:"), "{v:?}");
    }

    #[test]
    fn allowing_the_lint_outside_a_wrapper_is_caught() {
        let text = format!("#![{ALLOW}]\n");
        let f = files(&[
            ("crates/opus/src/lib.rs", &block()),
            ("crates/net/src/lib.rs", &text),
        ]);
        assert_eq!(violations(&f, &["crates/opus/"]).len(), 1);
    }

    #[test]
    fn every_form_of_the_keyword_counts_and_look_alikes_do_not() {
        for line in [
            format!("{KEYWORD} fn f() {{}}"),
            format!("{KEYWORD} impl Send for X {{}}"),
            format!("{KEYWORD} extern \"C\" {{}}"),
            format!("let x = {KEYWORD}{{ y }};"),
            format!("    {KEYWORD} {{"),
        ] {
            assert!(uses_it(&line), "{line}");
        }
        for line in [
            format!("// this is not {KEYWORD} at all"),
            format!("let {KEYWORD}_count = 3;"),
            format!("let is_{KEYWORD} = false;"),
            format!("fn f() {{}} // {KEYWORD} {{"),
            String::from("fn safe() {}"),
        ] {
            assert!(!uses_it(&line), "{line}");
        }
    }

    #[test]
    fn a_listed_wrapper_with_no_unsafe_code_is_a_violation_not_a_pass() {
        let f = files(&[("crates/opus/src/lib.rs", "fn f() {}")]);
        let v = violations(&f, &["crates/opus/"]);
        assert_eq!(v.len(), 1, "{v:?}");
        assert!(v[0].contains("update the guard"), "{v:?}");
        // And one that does not exist at all.
        assert_eq!(violations(&files(&[]), &["crates/gone/"]).len(), 1);
    }

    // ---- the real repository ----
    #[test]
    fn only_the_c_wrapper_crates_hold_unsafe_code() {
        let files = repo::files(&["rs"]).unwrap();
        assert!(
            files.len() >= 15,
            "only {} Rust files seen: nothing to check",
            files.len()
        );
        assert!(!C_WRAPPERS.is_empty());
        assert_eq!(violations(&files, C_WRAPPERS), Vec::<String>::new());
    }
}
