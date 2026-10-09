use std::fmt;

use zeroize::{Zeroize, Zeroizing};

/// A password, seed or other secret text. Every password and secret input
/// in moku is one of these, from the keystroke buffer to the KDF.
///
/// - The buffer is wiped on drop, including its spare capacity.
/// - Growing never leaves a stale copy behind: when it is full, `push`
///   moves the text into a bigger buffer itself and wipes the old one,
///   instead of letting `String` reallocate.
/// - `pop` overwrites the removed bytes.
/// - No `Clone` and no `Display`; `duplicate()` makes a visible copy,
///   `Debug` is redacted.
#[derive(Default)]
pub struct SecretString(Zeroizing<String>);

/// Capacity a growing buffer starts from, so typing a password of normal
/// length never moves it at all.
const MIN_GROWTH: usize = 64;

impl SecretString {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self(Zeroizing::new(String::with_capacity(capacity)))
    }

    fn reserve_wiping(&mut self, additional: usize) {
        let needed = self.0.len() + additional;
        if needed <= self.0.capacity() {
            return;
        }
        let capacity = needed.max(self.0.capacity() * 2).max(MIN_GROWTH);
        let mut bigger = Zeroizing::new(String::with_capacity(capacity));
        bigger.push_str(&self.0);
        // The old buffer is dropped (and wiped) here.
        self.0 = bigger;
    }

    pub fn push(&mut self, c: char) {
        self.reserve_wiping(c.len_utf8());
        self.0.push(c);
    }

    pub fn push_str(&mut self, s: &str) {
        self.reserve_wiping(s.len());
        self.0.push_str(s);
    }

    /// Removes the last character and overwrites its bytes.
    pub fn pop(&mut self) -> Option<char> {
        let c = self.0.pop()?;
        let len = self.0.len();
        // Writing NULs over the freed bytes stays within the capacity
        // they occupied, so this never reallocates.
        for _ in 0..c.len_utf8() {
            self.0.push('\0');
        }
        self.0.truncate(len);
        Some(c)
    }

    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Wipes the text (and the whole buffer) and leaves it empty.
    pub fn zeroize(&mut self) {
        self.0.zeroize();
    }

    /// Moves the text out, leaving this one empty. Lets a form hand its
    /// password on without copying it.
    pub fn take(&mut self) -> Self {
        std::mem::take(self)
    }

    /// An explicit, visible copy.
    pub fn duplicate(&self) -> Self {
        let mut copy = Self::with_capacity(self.0.len());
        copy.0.push_str(&self.0);
        copy
    }

    /// Drops trailing `\n`/`\r` characters (wiping them), for a secret
    /// read from a pipe.
    pub fn trim_line_ending(&mut self) {
        while self.0.ends_with(['\n', '\r']) {
            self.pop();
        }
    }
}

/// Takes ownership of the string's buffer; nothing is copied.
impl From<String> for SecretString {
    fn from(s: String) -> Self {
        Self(Zeroizing::new(s))
    }
}

/// Copies the text into a new wiped-on-drop buffer (literals, test
/// fixtures). The source `&str` itself is not wiped.
impl From<&str> for SecretString {
    fn from(s: &str) -> Self {
        let mut out = Self::with_capacity(s.len());
        out.0.push_str(s);
        out
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SecretString(***REDACTED***)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_push_pop_and_expose() {
        let mut s = SecretString::new();
        for c in "pässword".chars() {
            s.push(c);
        }
        assert_eq!(s.expose(), "pässword");
        assert_eq!(s.pop(), Some('d'));
        assert_eq!(s.expose(), "pässwor");
        s.push_str("d!");
        assert_eq!(s.expose(), "pässword!");
        assert_eq!(s.len(), "pässword!".len());
    }

    #[test]
    fn test_pop_overwrites_the_removed_bytes() {
        let mut s = SecretString::with_capacity(16);
        s.push_str("ab€");
        s.pop();
        assert_eq!(s.expose(), "ab");
        // The spare capacity that held '€' now holds NULs.
        s.0.push_str("\0\0\0");
        assert_eq!(&s.0.as_bytes()[2..5], &[0, 0, 0]);
    }

    #[test]
    fn test_take_leaves_it_empty_and_duplicate_copies() {
        let mut s = SecretString::from("hunter2".to_string());
        let copy = s.duplicate();
        let taken = s.take();
        assert!(s.is_empty());
        assert_eq!(taken.expose(), "hunter2");
        assert_eq!(copy.expose(), "hunter2");
    }

    #[test]
    fn test_trim_line_ending() {
        let mut s = SecretString::from("pw\r\n".to_string());
        s.trim_line_ending();
        assert_eq!(s.expose(), "pw");
    }

    #[test]
    fn test_zeroize_empties_it() {
        let mut s = SecretString::from("pw".to_string());
        s.zeroize();
        assert!(s.is_empty());
    }

    #[test]
    fn test_debug_is_redacted() {
        let s = SecretString::from("hunter2".to_string());
        assert_eq!(format!("{s:?}"), "SecretString(***REDACTED***)");
    }
}
