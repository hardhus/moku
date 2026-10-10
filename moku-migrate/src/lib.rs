//! The embedded, one-shot data-format migration chain.
//!
//! The data directory records its own format in a small `FORMAT` file
//! (TOML, next to `vault/`), readable without unlocking. When it is behind
//! [`CURRENT_DATA_FORMAT`], [`upgrade`] backs the data up and runs every
//! pending [`Hop`] in order, straight to the latest format, advancing the
//! marker after each hop. The user confirms once and never sees the
//! intermediate versions.
//!
//! The runtime crates only know the latest format. Everything that reads
//! an older one lives in this crate.
//!
//! # Rules for writing a hop
//!
//! - **A hop is frozen once it has shipped.** Its code, and the old-format
//!   types it reads, never change again. A bug found later is fixed by a
//!   new hop, not by editing an old one.
//! - **A hop owns its old-format types.** It declares its own copy of
//!   every struct/enum it reads (see `hops::h0000_key_scheme_v1`'s
//!   `Envelope`) and never imports a runtime type to read an old format:
//!   a later change to the runtime type would silently change the hop.
//! - **A hop is crash safe and idempotent.** It builds the new structure
//!   beside the old one and swaps it in atomically (`atomic_write`, a
//!   rename, a single sled insert per record), so a crash at any point
//!   leaves data the same hop can finish on the next run. Running a hop
//!   on data it already converted changes nothing.
//! - **A hop has a fixture test** derived from `test-fixtures/` (copied
//!   to a temp dir first; the fixtures themselves are frozen).
//! - **Adding a hop:** a new file in `src/hops/`, one entry at the end of
//!   [`HOPS`], bump [`CURRENT_DATA_FORMAT`], describe the new format in
//!   `format::components_of`, and add the fixture test.
//!
//! Volumes carry their own, separate counter (`volume.json`'s `format`),
//! upgraded lazily per volume at mount time: see [`volume`].

mod backup;
mod detect;
mod error;
mod format;
mod hop;
mod hops;
mod runner;
pub mod volume;

pub use backup::{BACKUPS_DIR, BackupInfo, create_backup, list_backups};
pub use detect::{DataState, detect_format, stamp_if_needed};
pub use error::{MigrateError, RecordFailure};
pub use format::{
    CURRENT_DATA_FORMAT, Components, DataFormat, FORMAT_FILE, FormatFile, read_format,
};
pub use hop::{HOPS, Hop, HopReport, MigrationCtx, Progress, pending_hops};
pub use runner::{UpgradeReport, upgrade};
