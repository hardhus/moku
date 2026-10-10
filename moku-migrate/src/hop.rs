use std::path::Path;

use moku_vault::VaultSession;

use crate::{DataFormat, MigrateError, hops};

/// One step of the data-format chain, from `from` to `to` (always
/// `from + 1`). See the crate docs for the rules a hop follows.
pub struct Hop {
    pub from: DataFormat,
    pub to: DataFormat,
    /// Whether the hop needs the unlocked root key.
    pub needs_unlock: bool,
    /// Shown to the user in the upgrade prompt and the CLI.
    pub description: &'static str,
    pub run: fn(&MigrationCtx<'_>) -> Result<HopReport, MigrateError>,
}

/// What a running hop gets: the data directory, the session (for hops that
/// need the root key) and a progress callback.
pub struct MigrationCtx<'a> {
    pub data_dir: &'a Path,
    pub session: &'a VaultSession,
    pub(crate) hop: &'static str,
    pub(crate) progress: &'a dyn Fn(Progress),
}

impl MigrationCtx<'_> {
    /// Reports how far the hop got (`module` being worked on, `records`
    /// converted so far in it).
    pub fn report(&self, module: &str, records: usize) {
        (self.progress)(Progress {
            hop: self.hop,
            module: module.to_string(),
            records,
        });
    }
}

/// A progress update, for the TUI's upgrade screen or CLI output.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Progress {
    pub hop: &'static str,
    pub module: String,
    pub records: usize,
}

/// What a hop did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HopReport {
    pub migrated: usize,
    pub skipped: usize,
    /// Records converted, per module (or other unit the hop works in).
    pub per_module: Vec<(String, usize)>,
}

/// The chain, in order. This list is the only source of truth for what
/// hops exist; `hops_are_contiguous` pins its shape.
pub static HOPS: &[Hop] = &[hops::h0000_key_scheme_v1::HOP];

/// The hops that take a directory in format `from` to `CURRENT_DATA_FORMAT`.
pub fn pending_hops(from: DataFormat) -> &'static [Hop] {
    pending_in(HOPS, from)
}

pub(crate) fn pending_in(hops: &[Hop], from: DataFormat) -> &[Hop] {
    match hops.iter().position(|h| h.from == from) {
        Some(i) => &hops[i..],
        None => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CURRENT_DATA_FORMAT;

    #[test]
    fn hops_are_contiguous() {
        assert_eq!(HOPS.first().map(|h| h.from), Some(DataFormat(0)));
        for h in HOPS {
            assert_eq!(
                h.to.0,
                h.from.0 + 1,
                "{} is not a single step",
                h.description
            );
        }
        for pair in HOPS.windows(2) {
            assert_eq!(pair[0].to, pair[1].from);
        }
        assert_eq!(HOPS.last().map(|h| h.to), Some(CURRENT_DATA_FORMAT));
    }

    #[test]
    fn pending_hops_start_at_the_given_format() {
        assert_eq!(pending_hops(DataFormat(0)).len(), HOPS.len());
        assert!(pending_hops(CURRENT_DATA_FORMAT).is_empty());
    }
}
