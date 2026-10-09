use std::fmt;

use zeroize::{Zeroize, ZeroizeOnDrop};

#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct SafeKey(pub [u8; 32]);

impl fmt::Debug for SafeKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SafeKey(***REDACTED***)")
    }
}
