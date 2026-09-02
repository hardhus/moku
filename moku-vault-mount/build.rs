fn main() {
    // `#[cfg(windows)]` here would reflect the HOST platform (build
    // scripts always run on the host, even when cross-compiling) — not
    // the actual compile TARGET. Checking CARGO_CFG_TARGET_OS instead is
    // what makes `cargo check --target x86_64-unknown-linux-gnu` work
    // from this (Windows) dev machine for the FUSE shim (plan Faz 6):
    // without this, winfsp's own build helper panics with "unsupported
    // triple" the moment it's invoked while cross-checking a non-Windows
    // target, since it unconditionally reads that same env var itself.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winfsp::build::winfsp_link_delayload();
    }
}
