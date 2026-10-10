//! Only moku-crypto may use the RustCrypto primitives directly.
//!
//! Keeping them in one crate is what lets the rules in CLAUDE.md (one
//! KDF, no raw master key as a cipher key, HKDF domain separation, AAD,
//! zeroization) be enforced by moku-crypto's types instead of by review,
//! and keeps a dependency upgrade (P10) to one crate.
//!
//! Removing a crate from a manifest already makes `use aes_gcm::..` fail
//! to compile elsewhere; this test stops it from being added back.

use std::path::{Path, PathBuf};

/// Crates that must only appear in moku-crypto's Cargo.toml.
const BANNED_CRATES: &[&str] = &[
    "aes",
    "aes-gcm",
    "aes-siv",
    "chacha20",
    "chacha20poly1305",
    "poly1305",
    "ghash",
    "polyval",
    "hkdf",
    "argon2",
];

/// Paths that must not appear in Rust code outside moku-crypto.
const BANNED_PATHS: &[&str] = &[
    "aes_gcm::",
    "aes_siv::",
    "chacha20poly1305::",
    "hkdf::",
    "argon2::",
];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("moku-crypto lives inside the workspace")
        .to_path_buf()
}

/// Every file under `dir` with the given name/extension, skipping build
/// output, VCS data and moku-crypto itself.
fn walk(dir: &Path, keep: &dyn Fn(&Path) -> bool, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if matches!(name.as_ref(), "target" | ".git" | "moku-crypto" | "node_modules")
                || name.starts_with('.')
            {
                continue;
            }
            walk(&path, keep, out);
        } else if keep(&path) {
            out.push(path);
        }
    }
}

/// The dependency keys declared in a manifest's `[dependencies]`-like
/// sections (`foo = ..`, `foo.workspace = true`, `[dependencies.foo]`).
fn declared_dependencies(manifest: &str) -> Vec<String> {
    let mut deps = Vec::new();
    let mut in_deps = false;
    for line in manifest.lines() {
        let line = line.trim();
        if let Some(section) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            in_deps = section.ends_with("dependencies");
            if let Some(dep) = section.rsplit_once("dependencies.").map(|(_, d)| d) {
                deps.push(dep.to_string());
            }
            continue;
        }
        if in_deps && !line.starts_with('#') {
            if let Some((key, _)) = line.split_once('=') {
                let key = key.trim();
                deps.push(key.split('.').next().unwrap_or(key).trim_matches('"').to_string());
            }
        }
    }
    deps
}

#[test]
fn no_other_manifest_depends_on_a_crypto_primitive() {
    let mut manifests = Vec::new();
    walk(
        &workspace_root(),
        &|p| p.file_name().is_some_and(|n| n == "Cargo.toml"),
        &mut manifests,
    );
    assert!(manifests.len() > 5, "the walk should find the workspace's manifests");

    let mut offenders = Vec::new();
    for manifest in manifests {
        let is_root = manifest.parent() == Some(workspace_root().as_path());
        let text = std::fs::read_to_string(&manifest).unwrap();
        for dep in declared_dependencies(&text) {
            // The root manifest only declares versions in
            // [workspace.dependencies]; using them is what's banned.
            if !is_root && BANNED_CRATES.contains(&dep.as_str()) {
                offenders.push(format!("{}: {dep}", manifest.display()));
            }
        }
    }
    assert!(offenders.is_empty(), "only moku-crypto may depend on: {offenders:#?}");
}

#[test]
fn no_other_source_file_uses_a_crypto_primitive() {
    let mut sources = Vec::new();
    walk(
        &workspace_root(),
        &|p| p.extension().is_some_and(|e| e == "rs"),
        &mut sources,
    );
    assert!(sources.len() > 20, "the walk should find the workspace's sources");

    let mut offenders = Vec::new();
    for source in sources {
        let text = std::fs::read_to_string(&source).unwrap();
        for banned in BANNED_PATHS {
            if text.contains(banned) {
                offenders.push(format!("{}: {banned}", source.display()));
            }
        }
    }
    assert!(offenders.is_empty(), "use moku-crypto instead of: {offenders:#?}");
}

#[test]
fn the_manifest_parser_sees_every_declaration_style() {
    let manifest = r#"
[package]
name = "x"
aes = "not a dependency"

[dependencies]
hkdf = "0.12"
argon2.workspace = true
"aes-gcm" = { version = "0.10" }

[dev-dependencies]
sha2 = "0.10"

[dependencies.polyval]
version = "0.6"
"#;
    let deps = declared_dependencies(manifest);
    for expected in ["hkdf", "argon2", "aes-gcm", "sha2", "polyval"] {
        assert!(deps.contains(&expected.to_string()), "{expected} in {deps:?}");
    }
    assert!(!deps.contains(&"aes".to_string()), "[package] keys are not dependencies");
}
