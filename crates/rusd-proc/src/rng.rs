//! Deterministic, path-keyed RNG (SPEC §5). Stream key =
//! `hash(stage.seed, prim_path, local_seed)`, so editing one branch never
//! reshuffles another. ChaCha8 (per SPEC); uniform / normal / chance helpers.

use std::hash::{DefaultHasher, Hash, Hasher};

use rand_chacha::ChaCha8Rng;
use rand_core::Rng as _;
use rand_core::SeedableRng;

/// A deterministic RNG stream.
pub struct Rng {
    inner: ChaCha8Rng,
}

impl Rng {
    /// Path-keyed stream: same (stage_seed, path, local_seed) ⇒ same sequence.
    pub fn seeded(stage_seed: u64, path: &str, local_seed: u64) -> Self {
        let mut h = DefaultHasher::new();
        stage_seed.hash(&mut h);
        path.hash(&mut h);
        local_seed.hash(&mut h);
        Self::from_seed(h.finish())
    }

    pub fn from_seed(seed: u64) -> Self {
        Self {
            inner: ChaCha8Rng::seed_from_u64(seed),
        }
    }

    /// Uniform in [0, 1).
    pub fn next_f64(&mut self) -> f64 {
        (self.inner.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Uniform in [a, b).
    pub fn uniform(&mut self, a: f64, b: f64) -> f64 {
        if b > a {
            a + (b - a) * self.next_f64()
        } else {
            a
        }
    }

    /// Normal (Box–Muller) with mean `mu`, std-dev `sigma`.
    pub fn normal(&mut self, mu: f64, sigma: f64) -> f64 {
        let u1 = self.next_f64().max(1e-12);
        let u2 = self.next_f64();
        mu + sigma * (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
    }

    /// Bernoulli trial: true with probability `p`.
    pub fn chance(&mut self, p: f64) -> bool {
        self.next_f64() < p
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_same_seed() {
        let mut a = Rng::seeded(7, "/World/City", 42);
        let mut b = Rng::seeded(7, "/World/City", 42);
        for _ in 0..100 {
            assert_eq!(a.next_f64().to_bits(), b.next_f64().to_bits());
        }
    }

    #[test]
    fn path_keyed_isolation() {
        let mut a = Rng::seeded(7, "/World/BlockA", 42);
        let mut b = Rng::seeded(7, "/World/BlockB", 42);
        let sa: Vec<u64> = (0..10).map(|_| a.next_f64().to_bits()).collect();
        let sb: Vec<u64> = (0..10).map(|_| b.next_f64().to_bits()).collect();
        assert_ne!(sa, sb, "different paths must yield different streams");
    }

    #[test]
    fn uniform_in_range() {
        let mut r = Rng::from_seed(1);
        for _ in 0..1000 {
            let v = r.uniform(0.8, 1.4);
            assert!((0.8..1.4).contains(&v));
        }
    }

    #[test]
    fn normal_finite_and_centered() {
        let mut r = Rng::from_seed(2);
        let samples: Vec<f64> = (0..5000).map(|_| r.normal(24.0, 10.0)).collect();
        assert!(samples.iter().all(|v| v.is_finite()));
        let mean = samples.iter().sum::<f64>() / samples.len() as f64;
        assert!((mean - 24.0).abs() < 1.0, "mean {mean} should be near 24");
    }
}
