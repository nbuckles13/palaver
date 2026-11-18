//! Seeded random source. Streams are derived per purpose so that adding a node or link does
//! not shift the draws seen by the others.

/// A small deterministic generator (SplitMix64). Not for cryptography.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// An independent stream for `label` (for example a node or link index).
    pub fn derive(&self, label: u64) -> Self {
        let mut r = Self(self.0 ^ label.wrapping_mul(0x9e37_79b9_7f4a_7c15));
        r.next_u64();
        r
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n`; returns 0 when `n` is 0.
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next_u64() % n }
    }

    /// True with probability `per_million` / 1,000,000.
    pub fn chance(&mut self, per_million: u32) -> bool {
        self.below(1_000_000) < u64::from(per_million)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_sequence() {
        let (mut a, mut b) = (Rng::new(7), Rng::new(7));
        assert!((0..100).all(|_| a.next_u64() == b.next_u64()));
    }

    #[test]
    fn derived_streams_differ_from_each_other_and_the_parent() {
        let root = Rng::new(7);
        let (mut a, mut b, mut p) = (root.derive(1), root.derive(2), root.clone());
        let (x, y, z) = (a.next_u64(), b.next_u64(), p.next_u64());
        assert!(x != y && x != z && y != z);
    }
}
