//! Seeded splitmix64. Every jitter draw in the harness comes from here, never from
//! a thread-local RNG, so a seed fully determines a run.

const GAMMA: u64 = 0x9E37_79B9_7F4A_7C15;
const MIX_1: u64 = 0xBF58_476D_1CE4_E5B9;
const MIX_2: u64 = 0x94D0_49BB_1331_11EB;
const STREAM_GAMMA: u64 = 0xD1B5_4A32_D192_ED03;

#[derive(Clone, Debug)]
pub struct Prng {
    state: u64,
}

impl Prng {
    pub const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    /// Per-stream PRNG derived from the scenario seed. Distinct indices give
    /// distinct sequences.
    pub fn derive(seed: u64, stream_index: usize) -> Self {
        let mut mixer = Self::new(seed ^ (stream_index as u64).wrapping_mul(STREAM_GAMMA));
        Self::new(mixer.next_u64())
    }

    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(GAMMA);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(MIX_1);
        z = (z ^ (z >> 27)).wrapping_mul(MIX_2);
        z ^ (z >> 31)
    }

    /// Uniform-ish draw in `range`. An empty range yields its start.
    pub fn next_range(&mut self, range: std::ops::Range<u64>) -> u64 {
        if range.end <= range.start {
            return range.start;
        }
        let span = range.end - range.start;
        range.start + self.next_u64() % span
    }

    pub fn next_bool(&mut self) -> bool {
        self.next_u64() & 1 == 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_sequence() {
        let mut a = Prng::new(42);
        let mut b = Prng::new(42);
        let first: Vec<u64> = (0..16).map(|_| a.next_u64()).collect();
        let second: Vec<u64> = (0..16).map(|_| b.next_u64()).collect();
        assert_eq!(first, second);
        assert!(first.windows(2).any(|w| w[0] != w[1]));
    }

    #[test]
    fn different_seeds_diverge() {
        let mut a = Prng::new(1);
        let mut b = Prng::new(2);
        assert_ne!(a.next_u64(), b.next_u64());
    }

    #[test]
    fn derived_streams_are_independent_and_stable() {
        let mut s0 = Prng::derive(9, 0);
        let mut s1 = Prng::derive(9, 1);
        assert_ne!(s0.next_u64(), s1.next_u64());

        let mut again = Prng::derive(9, 1);
        assert_eq!(Prng::derive(9, 1).next_u64(), again.next_u64());
    }

    #[test]
    fn range_stays_in_bounds() {
        let mut prng = Prng::new(0xDEAD_BEEF);
        for _ in 0..1000 {
            let value = prng.next_range(10..20);
            assert!((10..20).contains(&value));
        }
        assert_eq!(prng.next_range(5..5), 5);
        let (lo, hi) = (7u64, 3u64);
        assert_eq!(prng.next_range(lo..hi), lo);
    }
}
