/// Associated data for `seal`/`open`: a fixed domain string plus the
/// context fields the ciphertext belongs to (module, record id, header...).
///
/// The domain and every field are length-prefixed (`u32` little-endian),
/// so the encoding is unambiguous: `["ab", "c"]` and `["a", "bc"]` differ.
/// A ciphertext opened with a different domain or any different field
/// fails authentication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Aad(Vec<u8>);

impl Aad {
    /// `domain` names the format, e.g. `"moku/secrets/export/v2"`.
    pub fn new(domain: &'static str) -> Self {
        let mut aad = Self(Vec::new());
        aad.push(domain.as_bytes());
        aad
    }

    pub fn field(mut self, bytes: &[u8]) -> Self {
        self.push(bytes);
        self
    }

    fn push(&mut self, bytes: &[u8]) {
        let len = u32::try_from(bytes.len()).expect("an AAD field is far below 4 GiB");
        self.0.extend_from_slice(&len.to_le_bytes());
        self.0.extend_from_slice(bytes);
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fields_are_unambiguous() {
        let a = Aad::new("d").field(b"ab").field(b"c");
        let b = Aad::new("d").field(b"a").field(b"bc");
        assert_ne!(a, b);
    }

    #[test]
    fn test_domain_is_part_of_it() {
        assert_ne!(Aad::new("x").field(b"1"), Aad::new("y").field(b"1"));
        assert_ne!(Aad::new("x"), Aad::new("x").field(b""));
    }

    #[test]
    fn test_encoding_is_pinned() {
        assert_eq!(
            Aad::new("ab").field(b"c").as_bytes(),
            b"\x02\x00\x00\x00ab\x01\x00\x00\x00c"
        );
    }
}
