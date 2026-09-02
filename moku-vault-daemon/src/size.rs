use anyhow::{Result, anyhow};

/// Parses a human-readable size like "10GB", "512MiB", or "1024" (plain
/// bytes) into a byte count. Accepts an optional decimal fraction
/// ("1.5GB"). Both decimal (KB/MB/GB/TB, 1000-based) and binary
/// (KiB/MiB/GiB/TiB, 1024-based) suffixes are accepted, case-insensitively.
pub fn parse_size(input: &str) -> Result<u64> {
    let s = input.trim();
    if s.is_empty() {
        return Err(anyhow!("size cannot be empty"));
    }
    let lower = s.to_ascii_lowercase();
    let split_at = lower.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(lower.len());
    let (number_part, unit_part) = (&lower[..split_at], lower[split_at..].trim());
    let number: f64 = number_part.parse().map_err(|_| anyhow!("invalid size number: '{input}'"))?;
    if number < 0.0 {
        return Err(anyhow!("size cannot be negative"));
    }

    let multiplier: f64 = match unit_part {
        "" | "b" => 1.0,
        "kb" => 1_000.0,
        "kib" => 1024.0,
        "mb" => 1_000_000.0,
        "mib" => 1024.0 * 1024.0,
        "gb" => 1_000_000_000.0,
        "gib" => 1024.0 * 1024.0 * 1024.0,
        "tb" => 1_000_000_000_000.0,
        "tib" => 1024.0_f64.powi(4),
        other => {
            return Err(anyhow!(
                "unknown size unit '{other}' in '{input}' (expected B/KB/MB/GB/TB or KiB/MiB/GiB/TiB)"
            ));
        }
    };

    Ok((number * multiplier).round() as u64)
}

/// Formats a byte count back into a compact human-readable binary size,
/// for `moku vault list`/`status` display.
pub fn format_size(bytes: u64) -> String {
    const UNITS: &[(&str, f64)] =
        &[("TiB", 1024.0_f64 * 1024.0 * 1024.0 * 1024.0), ("GiB", 1024.0 * 1024.0 * 1024.0), ("MiB", 1024.0 * 1024.0), ("KiB", 1024.0)];
    let b = bytes as f64;
    for (unit, size) in UNITS {
        if b >= *size {
            return format!("{:.2} {unit}", b / size);
        }
    }
    format!("{bytes} B")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_plain_bytes() {
        assert_eq!(parse_size("1024").unwrap(), 1024);
    }

    #[test]
    fn test_parse_decimal_gb() {
        assert_eq!(parse_size("10GB").unwrap(), 10_000_000_000);
    }

    #[test]
    fn test_parse_binary_gib() {
        assert_eq!(parse_size("1GiB").unwrap(), 1024 * 1024 * 1024);
    }

    #[test]
    fn test_parse_case_insensitive_and_fractional() {
        assert_eq!(parse_size("1.5gb").unwrap(), 1_500_000_000);
    }

    #[test]
    fn test_parse_tolerates_internal_space() {
        assert_eq!(parse_size("10 GB").unwrap(), 10_000_000_000);
    }

    #[test]
    fn test_parse_rejects_unknown_unit() {
        assert!(parse_size("10XB").is_err());
    }

    #[test]
    fn test_parse_rejects_empty() {
        assert!(parse_size("").is_err());
    }

    #[test]
    fn test_format_size_roundtrip_scale() {
        assert_eq!(format_size(1024 * 1024 * 1024), "1.00 GiB");
    }

    #[test]
    fn test_format_size_small_value_in_bytes() {
        assert_eq!(format_size(500), "500 B");
    }
}
