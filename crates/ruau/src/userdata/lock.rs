pub trait UserDataLock {
    fn is_locked(&self) -> bool;
    fn try_lock_shared(&self) -> bool;
    fn try_lock_exclusive(&self) -> bool;

    unsafe fn unlock_shared(&self);
    unsafe fn unlock_exclusive(&self);

    fn try_lock_shared_guarded(&self) -> Result<LockGuard<'_, Self>, ()> {
        if self.try_lock_shared() {
            Ok(LockGuard {
                lock: self,
                exclusive: false,
            })
        } else {
            Err(())
        }
    }

    fn try_lock_exclusive_guarded(&self) -> Result<LockGuard<'_, Self>, ()> {
        if self.try_lock_exclusive() {
            Ok(LockGuard {
                lock: self,
                exclusive: true,
            })
        } else {
            Err(())
        }
    }
}

pub struct LockGuard<'a, L: UserDataLock + ?Sized> {
    lock: &'a L,
    exclusive: bool,
}

impl<L: UserDataLock + ?Sized> Drop for LockGuard<'_, L> {
    fn drop(&mut self) {
        unsafe {
            if self.exclusive {
                self.lock.unlock_exclusive();
            } else {
                self.lock.unlock_shared();
            }
        }
    }
}

pub use lock_impl::{RawLock, RwLock};

#[cfg(not(tarpaulin_include))]
mod lock_impl {
    use std::cell::{Cell, UnsafeCell};

    // Positive values represent the number of read references.
    // Negative values represent the number of write references (only one allowed).
    pub type RawLock = Cell<isize>;

    const UNUSED: isize = 0;

    impl super::UserDataLock for RawLock {
        #[inline(always)]
        fn is_locked(&self) -> bool {
            self.get() != UNUSED
        }

        #[inline(always)]
        fn try_lock_shared(&self) -> bool {
            let flag = self
                .get()
                .checked_add(1)
                .expect("userdata lock count overflow");
            if flag <= UNUSED {
                return false;
            }
            self.set(flag);
            true
        }

        #[inline(always)]
        fn try_lock_exclusive(&self) -> bool {
            let flag = self.get();
            if flag != UNUSED {
                return false;
            }
            self.set(UNUSED - 1);
            true
        }

        #[inline(always)]
        unsafe fn unlock_shared(&self) {
            let flag = self.get();
            debug_assert!(flag > UNUSED);
            self.set(flag - 1);
        }

        #[inline(always)]
        unsafe fn unlock_exclusive(&self) {
            let flag = self.get();
            debug_assert!(flag < UNUSED);
            self.set(flag + 1);
        }
    }

    /// A cheap single-threaded read-write lock for userdata borrow tracking.
    pub struct RwLock<T> {
        lock: RawLock,
        data: UnsafeCell<T>,
    }

    impl<T> RwLock<T> {
        /// Creates a new `RwLock` containing the given value.
        #[inline(always)]
        pub(crate) fn new(value: T) -> Self {
            Self {
                lock: RawLock::new(UNUSED),
                data: UnsafeCell::new(value),
            }
        }

        /// Returns a reference to the underlying raw lock.
        #[inline(always)]
        pub(crate) unsafe fn raw(&self) -> &RawLock {
            &self.lock
        }

        /// Returns a raw pointer to the underlying data.
        #[inline(always)]
        pub(crate) fn data_ptr(&self) -> *mut T {
            self.data.get()
        }

        /// Consumes this `RwLock`, returning the underlying data.
        #[inline(always)]
        pub(crate) fn into_inner(self) -> T {
            self.data.into_inner()
        }
    }
}
