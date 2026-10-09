use std::alloc::{Layout, dealloc};
use std::ops::{Deref, DerefMut};
use std::ptr::NonNull;
use std::sync::atomic::{Ordering, compiler_fence};

/// A boxed value whose memory is overwritten with zeros after it is
/// dropped and before it goes back to the allocator.
///
/// For library cipher states that don't (reliably) wipe themselves:
/// polyval 0.6.2's runtime-detected backend never drops its GHASH state
/// (see tests/zeroize_on_free.rs). Wiping the raw bytes covers any such
/// type regardless of its own `Drop`.
///
/// Only sound for types whose secrets live inline, with no heap pointers
/// of their own (true for every RustCrypto cipher state used here); heap
/// memory a `T` owns would be freed by its own `Drop` without this wipe.
pub(crate) struct WipeOnDrop<T> {
    ptr: NonNull<T>,
}

impl<T> WipeOnDrop<T> {
    pub(crate) fn new(value: T) -> Self {
        Self {
            ptr: NonNull::from(Box::leak(Box::new(value))),
        }
    }
}

impl<T> Deref for WipeOnDrop<T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: `ptr` came from `Box::leak` and stays valid until `drop`.
        unsafe { self.ptr.as_ref() }
    }
}

impl<T> DerefMut for WipeOnDrop<T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as above, and `&mut self` makes this the only reference.
        unsafe { self.ptr.as_mut() }
    }
}

impl<T> Drop for WipeOnDrop<T> {
    fn drop(&mut self) {
        let layout = Layout::new::<T>();
        let raw = self.ptr.as_ptr();
        // SAFETY: `raw` is the live, uniquely owned allocation made by
        // `Box::new` in `new`, with `T`'s layout. After `drop_in_place`
        // the bytes are dead, so overwriting them is fine; then the block
        // is freed with the same layout `Box` used. A zero-sized `T` was
        // never allocated and is not freed.
        unsafe {
            std::ptr::drop_in_place(raw);
            let bytes = raw.cast::<u8>();
            for i in 0..layout.size() {
                std::ptr::write_volatile(bytes.add(i), 0);
            }
            compiler_fence(Ordering::SeqCst);
            if layout.size() != 0 {
                dealloc(bytes, layout);
            }
        }
    }
}

// SAFETY: `WipeOnDrop<T>` owns its `T` exactly like `Box<T>` does.
unsafe impl<T: Send> Send for WipeOnDrop<T> {}
unsafe impl<T: Sync> Sync for WipeOnDrop<T> {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn test_deref_and_drop_run_the_inner_drop() {
        static DROPS: AtomicUsize = AtomicUsize::new(0);
        struct Counted([u8; 8]);
        impl Drop for Counted {
            fn drop(&mut self) {
                DROPS.fetch_add(1, Ordering::SeqCst);
            }
        }
        let mut w = WipeOnDrop::new(Counted([1; 8]));
        w.0[0] = 9;
        assert_eq!(w.0[0], 9);
        drop(w);
        assert_eq!(DROPS.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_zero_sized_values_are_fine() {
        drop(WipeOnDrop::new(()));
    }
}
