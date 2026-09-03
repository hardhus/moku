use anyhow::{Result, anyhow};
use rand::Rng;
use rand::rngs::OsRng;

/// The EFF "large" diceware wordlist (7776 words) — the standard,
/// well-audited list. A larger/custom list is a possible future addition
/// (see the plan's backlog note); this enum is the seam for it.
const EFF_LARGE_WORDLIST: &str = include_str!("../assets/eff_large_wordlist.txt");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wordlist {
    EffLarge,
}

impl Wordlist {
    fn words(self) -> Vec<&'static str> {
        match self {
            Self::EffLarge => EFF_LARGE_WORDLIST.lines().collect(),
        }
    }
}

const LOWERCASE: &[u8] = b"abcdefghijklmnopqrstuvwxyz";
const UPPERCASE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const DIGITS: &[u8] = b"0123456789";
// Excludes characters that commonly cause quoting/escaping trouble
// (backslash, backtick, quotes) — still a large, varied symbol set.
const SYMBOLS: &[u8] = b"!@#$%^&*()-_=+[]{};:,.?/~";

#[derive(Clone, Debug)]
pub struct CharsetOptions {
    pub length: usize,
    pub lowercase: bool,
    pub uppercase: bool,
    pub digits: bool,
    pub symbols: bool,
}

impl Default for CharsetOptions {
    fn default() -> Self {
        Self { length: 20, lowercase: true, uppercase: true, digits: true, symbols: true }
    }
}

fn charset_pool(opts: &CharsetOptions) -> Vec<u8> {
    let mut pool = Vec::new();
    if opts.lowercase {
        pool.extend_from_slice(LOWERCASE);
    }
    if opts.uppercase {
        pool.extend_from_slice(UPPERCASE);
    }
    if opts.digits {
        pool.extend_from_slice(DIGITS);
    }
    if opts.symbols {
        pool.extend_from_slice(SYMBOLS);
    }
    pool
}

pub fn generate_charset_password(opts: &CharsetOptions) -> Result<String> {
    if opts.length == 0 {
        return Err(anyhow!("length must be at least 1"));
    }
    let pool = charset_pool(opts);
    if pool.is_empty() {
        return Err(anyhow!("at least one character set must be enabled"));
    }
    let mut rng = OsRng;
    let password: String = (0..opts.length).map(|_| pool[rng.gen_range(0..pool.len())] as char).collect();
    Ok(password)
}

pub fn charset_entropy_bits(opts: &CharsetOptions) -> f64 {
    let pool_size = charset_pool(opts).len();
    if pool_size == 0 {
        return 0.0;
    }
    (pool_size as f64).log2() * opts.length as f64
}

#[derive(Clone, Debug)]
pub struct DicewareOptions {
    pub word_count: usize,
    pub separator: String,
    pub capitalize: bool,
    pub add_number: bool,
    pub wordlist: Wordlist,
}

impl Default for DicewareOptions {
    fn default() -> Self {
        Self { word_count: 6, separator: "-".to_string(), capitalize: false, add_number: true, wordlist: Wordlist::EffLarge }
    }
}

pub fn generate_diceware(opts: &DicewareOptions) -> Result<String> {
    if opts.word_count == 0 {
        return Err(anyhow!("word_count must be at least 1"));
    }
    let words = opts.wordlist.words();
    if words.is_empty() {
        return Err(anyhow!("wordlist is empty"));
    }
    let mut rng = OsRng;
    let mut parts: Vec<String> = (0..opts.word_count)
        .map(|_| {
            let w = words[rng.gen_range(0..words.len())];
            if opts.capitalize { capitalize(w) } else { w.to_string() }
        })
        .collect();
    if opts.add_number {
        parts.push(rng.gen_range(0..100).to_string());
    }
    Ok(parts.join(&opts.separator))
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Entropy of a diceware passphrase in bits. `add_number` contributes
/// log2(100) ≈ 6.64 bits, matching `generate_diceware`'s `0..100` draw.
pub fn diceware_entropy_bits(word_count: usize, wordlist: Wordlist, add_number: bool) -> f64 {
    let pool_size = wordlist.words().len();
    if pool_size == 0 {
        return 0.0;
    }
    let mut bits = (pool_size as f64).log2() * word_count as f64;
    if add_number {
        bits += (100f64).log2();
    }
    bits
}

pub fn strength_label(bits: f64) -> &'static str {
    match bits {
        b if b < 28.0 => "very weak",
        b if b < 36.0 => "weak",
        b if b < 60.0 => "reasonable",
        b if b < 128.0 => "strong",
        _ => "very strong",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wordlist_has_7776_unique_words() {
        let words = Wordlist::EffLarge.words();
        assert_eq!(words.len(), 7776);
        let unique: std::collections::HashSet<_> = words.iter().collect();
        assert_eq!(unique.len(), 7776, "wordlist must not contain duplicates");
        // The real list includes a handful of hyphenated compound words
        // (drop-down, felt-tip, t-shirt, yo-yo) alongside plain lowercase
        // ones.
        assert!(words.iter().all(|w| !w.is_empty() && w.chars().all(|c| c.is_ascii_lowercase() || c == '-')));
    }

    #[test]
    fn test_charset_password_has_requested_length() {
        let opts = CharsetOptions { length: 16, ..Default::default() };
        let pw = generate_charset_password(&opts).unwrap();
        assert_eq!(pw.chars().count(), 16);
    }

    #[test]
    fn test_charset_password_respects_disabled_sets() {
        let opts = CharsetOptions { length: 50, lowercase: false, uppercase: true, digits: false, symbols: false };
        let pw = generate_charset_password(&opts).unwrap();
        assert!(pw.chars().all(|c| c.is_ascii_uppercase()));
    }

    #[test]
    fn test_charset_password_no_charset_enabled_errors() {
        let opts = CharsetOptions { length: 10, lowercase: false, uppercase: false, digits: false, symbols: false };
        assert!(generate_charset_password(&opts).is_err());
    }

    #[test]
    fn test_charset_entropy_known_value() {
        // 26 lowercase letters, length 10: log2(26) * 10 ≈ 47.0 bits.
        let opts = CharsetOptions { length: 10, lowercase: true, uppercase: false, digits: false, symbols: false };
        let bits = charset_entropy_bits(&opts);
        assert!((bits - 47.0).abs() < 0.5, "expected ~47 bits, got {bits}");
    }

    #[test]
    fn test_diceware_word_count_and_separator() {
        let opts = DicewareOptions { word_count: 5, separator: "-".to_string(), capitalize: false, add_number: false, wordlist: Wordlist::EffLarge };
        let phrase = generate_diceware(&opts).unwrap();
        assert_eq!(phrase.split('-').count(), 5);
    }

    #[test]
    fn test_diceware_capitalize_and_number() {
        let opts = DicewareOptions { word_count: 3, separator: "-".to_string(), capitalize: true, add_number: true, wordlist: Wordlist::EffLarge };
        let phrase = generate_diceware(&opts).unwrap();
        let parts: Vec<&str> = phrase.split('-').collect();
        assert_eq!(parts.len(), 4); // 3 words + trailing number
        for w in &parts[..3] {
            assert!(w.chars().next().unwrap().is_uppercase());
        }
        assert!(parts[3].parse::<u32>().is_ok());
    }

    #[test]
    fn test_diceware_entropy_known_value() {
        // log2(7776) * 6 ≈ 77.55 bits, no trailing number.
        let bits = diceware_entropy_bits(6, Wordlist::EffLarge, false);
        assert!((bits - 77.5).abs() < 0.5, "expected ~77.5 bits, got {bits}");
    }

    #[test]
    fn test_strength_label_thresholds() {
        assert_eq!(strength_label(10.0), "very weak");
        assert_eq!(strength_label(30.0), "weak");
        assert_eq!(strength_label(45.0), "reasonable");
        assert_eq!(strength_label(90.0), "strong");
        assert_eq!(strength_label(150.0), "very strong");
    }
}
