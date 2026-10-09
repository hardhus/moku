//! The one source of randomness for anything cryptographic: the OS RNG.
//!
//! These panic if the OS RNG fails, which is what `rand`'s `OsRng` does
//! too: there is no meaningful way to continue without randomness.

use rand::RngCore;
use rand::rngs::OsRng;

pub fn fill_random(buf: &mut [u8]) {
    OsRng.fill_bytes(buf);
}

pub fn random_array<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    fill_random(&mut out);
    out
}

/// A random 16-byte identifier: a volume file id, a directory IV.
pub fn random_id16() -> [u8; 16] {
    random_array()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_random_values_differ() {
        assert_ne!(random_id16(), random_id16());
        assert_ne!(random_array::<32>(), random_array::<32>());
    }
}
