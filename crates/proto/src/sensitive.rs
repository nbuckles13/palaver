//! Wrappers that make leaking sensitive values hard (guards, mechanism 1: types).
//!
//! - [`Secret`] holds key material and passwords. It has no `Display` and no serialisation, and
//!   its `Debug` prints a fixed placeholder, so a struct holding one can still derive `Debug`.
//! - [`Redacted`] holds personal data (names, addresses, meeting ids). `Debug` and `Display`
//!   print a placeholder; the value is reached only through [`Redacted::reveal`].
//!
//! Reading the inner value is always an explicit, greppable call.

use core::fmt;

/// Key material or a password. Never printed, never serialised.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret<T>(T);

impl<T> Secret<T> {
    pub fn new(value: T) -> Self {
        Self(value)
    }

    /// The inner value. Call sites are the audit surface for secret handling.
    pub fn expose(&self) -> &T {
        &self.0
    }
}

impl<T> fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(..)")
    }
}

/// Personal data that must not reach logs or metric labels unredacted.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Redacted<T>(T);

impl<T> Redacted<T> {
    pub fn new(value: T) -> Self {
        Self(value)
    }

    /// The inner value, for showing to the user or putting on the wire.
    pub fn reveal(&self) -> &T {
        &self.0
    }
}

impl<T> fmt::Debug for Redacted<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

impl<T> fmt::Display for Redacted<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct Holder {
        _key: Secret<[u8; 4]>,
        _name: Redacted<String>,
    }

    #[test]
    fn secret_debug_does_not_contain_the_value() {
        let s = Secret::new(String::from("hunter2"));
        assert_eq!(format!("{s:?}"), "Secret(..)");
        assert_eq!(s.expose(), "hunter2");
    }

    #[test]
    fn redacted_debug_and_display_do_not_contain_the_value() {
        let r = Redacted::new(String::from("Alice"));
        assert_eq!(format!("{r:?}"), "[redacted]");
        assert_eq!(format!("{r}"), "[redacted]");
        assert_eq!(r.reveal(), "Alice");
    }

    #[test]
    fn a_struct_holding_both_can_derive_debug_without_leaking() {
        let h = Holder {
            _key: Secret::new([1, 2, 3, 4]),
            _name: Redacted::new(String::from("Alice")),
        };
        let out = format!("{h:?}");
        assert!(!out.contains("Alice"), "{out}");
        assert!(!out.contains('3'), "{out}");
    }
}
