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
