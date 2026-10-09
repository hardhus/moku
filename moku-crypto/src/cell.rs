use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::{CryptoError, Key, KeyRole};

/// Holds a key that can be wiped at any moment (lock, panic lock).
///
/// The key is never handed out: code borrows it for the length of a
/// closure (`with`), so nothing can keep a copy alive past `wipe`. `wipe`
/// waits for running `with` calls to finish, then zeroizes the key in
/// place; every later `with` gets `CryptoError::Locked`.
///
/// Don't do slow work inside `with` (no IO, no Argon2): derive what you
/// need (e.g. a module key) and let go. Long jobs can read `generation()`
/// at the start and compare at the end to notice a lock/unlock in between.
pub struct KeyCell<P: KeyRole> {
    inner: RwLock<Option<Key<P>>>,
    generation: AtomicU64,
}

impl<P: KeyRole> KeyCell<P> {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(None),
            generation: AtomicU64::new(0),
        }
    }

    // A panic inside some `with` closure must never stop the key from
    // being wiped later, so a poisoned lock is simply used as is: the
    // data (an `Option<Key>`) can't be left half-updated.
    fn read(&self) -> RwLockReadGuard<'_, Option<Key<P>>> {
        self.inner.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> RwLockWriteGuard<'_, Option<Key<P>>> {
        self.inner.write().unwrap_or_else(|e| e.into_inner())
    }

    /// Stores `key`, wiping any previous one.
    pub fn set(&self, key: Key<P>) {
        *self.write() = Some(key);
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    /// Zeroizes the key in place. Blocks until running `with` calls end.
    pub fn wipe(&self) {
        // Dropping the old `Key` zeroizes it.
        *self.write() = None;
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    pub fn is_set(&self) -> bool {
        self.read().is_some()
    }

    /// Runs `f` with the key, or returns `Locked` if there is none.
    pub fn with<R>(&self, f: impl FnOnce(&Key<P>) -> R) -> Result<R, CryptoError> {
        match &*self.read() {
            Some(key) => Ok(f(key)),
            None => Err(CryptoError::Locked),
        }
    }

    /// Changes on every `set` and `wipe`.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }
}

impl<P: KeyRole> Default for KeyCell<P> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RootKey;
    use std::sync::{Arc, Barrier};
    use std::time::Duration;

    fn key(b: u8) -> Key<RootKey> {
        Key::from_slice(&[b; 32]).unwrap()
    }

    #[test]
    fn test_set_with_wipe() {
        let cell = KeyCell::<RootKey>::new();
        assert!(!cell.is_set());
        assert_eq!(cell.with(|_| ()), Err(CryptoError::Locked));
        cell.set(key(1));
        assert!(cell.is_set());
        assert_eq!(cell.with(|k| k.expose()[0]), Ok(1));
        cell.wipe();
        assert!(!cell.is_set());
        assert_eq!(cell.with(|_| ()), Err(CryptoError::Locked));
    }

    #[test]
    fn test_generation_changes_on_set_and_wipe() {
        let cell = KeyCell::<RootKey>::new();
        let g0 = cell.generation();
        cell.set(key(1));
        let g1 = cell.generation();
        cell.wipe();
        assert!(g0 < g1 && g1 < cell.generation());
    }

    #[test]
    fn test_wipe_waits_for_a_running_with() {
        let cell = Arc::new(KeyCell::<RootKey>::new());
        cell.set(key(7));
        let inside = Arc::new(Barrier::new(2));
        let reader = {
            let (cell, inside) = (Arc::clone(&cell), Arc::clone(&inside));
            std::thread::spawn(move || {
                cell.with(|k| {
                    inside.wait();
                    std::thread::sleep(Duration::from_millis(100));
                    // Still intact: wipe is blocked until we return.
                    k.expose()[0]
                })
            })
        };
        inside.wait();
        cell.wipe();
        assert_eq!(reader.join().unwrap(), Ok(7));
        assert_eq!(cell.with(|_| ()), Err(CryptoError::Locked));
    }

    #[test]
    fn test_a_panic_inside_with_does_not_block_wipe() {
        let cell = Arc::new(KeyCell::<RootKey>::new());
        cell.set(key(1));
        let c = Arc::clone(&cell);
        let _ = std::thread::spawn(move || c.with(|_| panic!("boom"))).join();
        cell.wipe();
        assert!(!cell.is_set());
    }
}
