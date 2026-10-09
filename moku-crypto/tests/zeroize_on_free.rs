//! Proves, rather than assumes, that secret memory is wiped before it is
//! handed back to the allocator.
//!
//! This test binary installs its own global allocator. While a thread has
//! "armed" it, every deallocation in a chosen size range is scanned first:
//! a freed block that still contains a non-zero byte counts as dirty. A
//! test runs the code under test inside `track`, which drops everything it
//! allocated before disarming, and then asserts on the counts.
//!
//! Two kinds of tests live here:
//! - `library_fact_*`: what a RustCrypto crate does on its own, at the
//!   version in Cargo.lock. Some of these assert that the library does NOT
//!   wipe something; that is why moku-crypto wraps it. If a dependency
//!   upgrade (P10) makes one fail, re-check the wrapper: it may no longer
//!   be needed.
//! - everything else: moku-crypto's own types, which must always wipe.
//!
//! Only heap memory is checked. Stack copies can't be observed this way;
//! keeping secrets in boxes is how moku-crypto keeps them off the stack.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

struct CheckingAlloc;

thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
    static MIN_SIZE: Cell<usize> = const { Cell::new(0) };
    static MAX_SIZE: Cell<usize> = const { Cell::new(usize::MAX) };
    static CHECKED: Cell<usize> = const { Cell::new(0) };
    static DIRTY: Cell<usize> = const { Cell::new(0) };
}

unsafe impl GlobalAlloc for CheckingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // `try_with`: thread-locals may already be gone while a thread
        // shuts down; such frees are simply not checked.
        let _ = ARMED.try_with(|armed| {
            if !armed.get() {
                return;
            }
            let size = layout.size();
            if size < MIN_SIZE.get() || size > MAX_SIZE.get() {
                return;
            }
            // SAFETY: the block is still allocated and `size` bytes long.
            let bytes = unsafe { std::slice::from_raw_parts(ptr, size) };
            CHECKED.set(CHECKED.get() + 1);
            if bytes.iter().any(|&b| b != 0) {
                DIRTY.set(DIRTY.get() + 1);
            }
        });
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOC: CheckingAlloc = CheckingAlloc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Frees {
    checked: usize,
    dirty: usize,
}

/// Runs `f` with this thread's checker armed for frees whose size is in
/// `min..=max`. `f` must drop everything it allocates before returning.
fn track(min: usize, max: usize, f: impl FnOnce()) -> Frees {
    MIN_SIZE.set(min);
    MAX_SIZE.set(max);
    CHECKED.set(0);
    DIRTY.set(0);
    ARMED.set(true);
    f();
    ARMED.set(false);
    Frees {
        checked: CHECKED.get(),
        dirty: DIRTY.get(),
    }
}

const MIB: usize = 1024 * 1024;

#[test]
fn checker_detects_a_dirty_free() {
    let frees = track(MIB, usize::MAX, || drop(vec![1u8; 2 * MIB]));
    assert_eq!(frees, Frees { checked: 1, dirty: 1 });
}

#[test]
fn checker_accepts_a_wiped_free() {
    let frees = track(MIB, usize::MAX, || {
        let mut v = vec![1u8; 2 * MIB];
        zeroize::Zeroize::zeroize(&mut v);
    });
    assert_eq!(frees, Frees { checked: 1, dirty: 0 });
}

mod library_facts {
    use super::*;
    use aes_gcm::{Aes256Gcm, KeyInit};
    use argon2::{Algorithm, Argon2, Params, Version};
    use chacha20poly1305::XChaCha20Poly1305;

    /// argon2 0.5.3: even with its `zeroize` feature, `hash_password_into`
    /// frees its block memory (m KiB, password-derived state) unwiped.
    /// moku-crypto's `password_key` passes its own wiped buffer instead.
    #[test]
    fn library_fact_argon2_hash_password_into_does_not_wipe_its_memory() {
        let params = Params::new(1024, 1, 1, Some(32)).unwrap();
        let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
        let frees = track(MIB, usize::MAX, || {
            let mut out = [0u8; 32];
            argon2
                .hash_password_into(b"password", &[1u8; 16], &mut out)
                .unwrap();
        });
        assert_eq!(frees.checked, 1, "the 1 MiB block buffer should be seen");
        assert_eq!(frees.dirty, 1);
    }

    /// polyval 0.6.2's runtime-detected backend keeps its state in a
    /// `ManuallyDrop` union and never drops it, so a dropped `Aes256Gcm`
    /// leaves the GHASH key behind even with every `zeroize` feature on.
    /// moku-crypto keeps cipher states in `WipeOnDrop` instead.
    #[test]
    fn library_fact_aes256gcm_drop_leaves_ghash_state() {
        let size = std::mem::size_of::<Aes256Gcm>();
        let frees = track(size, size, || {
            let cipher = Box::new(Aes256Gcm::new_from_slice(&[7u8; 32]).unwrap());
            drop(cipher);
        });
        assert_eq!(frees.checked, 1);
        assert_eq!(frees.dirty, 1);
    }

    /// aes 0.8.4 with `zeroize` does wipe its round keys on drop, so the
    /// leftover in the test above is the GHASH state, not AES.
    #[test]
    fn library_fact_aes256_wipes_its_round_keys_on_drop() {
        let size = std::mem::size_of::<aes::Aes256>();
        let frees = track(size, size, || {
            let cipher = Box::new(aes::Aes256::new_from_slice(&[7u8; 32]).unwrap());
            drop(cipher);
        });
        assert_eq!(frees, Frees { checked: 1, dirty: 0 });
    }

    /// chacha20poly1305 0.10.1 wipes its key on drop by itself.
    #[test]
    fn library_fact_xchacha20poly1305_wipes_its_key_on_drop() {
        let size = std::mem::size_of::<XChaCha20Poly1305>();
        let frees = track(size, size, || {
            let cipher = Box::new(XChaCha20Poly1305::new_from_slice(&[7u8; 32]).unwrap());
            drop(cipher);
        });
        assert_eq!(frees, Frees { checked: 1, dirty: 0 });
    }
}
