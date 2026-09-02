fn main() {
    // A dependency's build script's `cargo:rustc-link-arg` output does not
    // propagate to a downstream binary crate — only to that dependency's
    // own tests/examples (moku-vault-mount's own real-mount test already
    // gets this "for free"). moku-bin is the actual final binary that
    // ships WinFsp support, so it must call this itself, or every `moku`
    // invocation hard-requires winfsp-x64.dll to be resolvable via the
    // normal DLL search order at process load — which it isn't, since
    // WinFsp's installer doesn't add itself to PATH — instead of only
    // when a vault is actually mounted, via WinFsp's own registry-based
    // lookup at that point (see winfsp::init::load_system_winfsp).
    // See moku-vault-mount/build.rs for why this checks the compile
    // TARGET (via CARGO_CFG_TARGET_OS) rather than `#[cfg(windows)]`,
    // which would reflect the host instead.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winfsp::build::winfsp_link_delayload();
    }
}
