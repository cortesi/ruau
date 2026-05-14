use std::cell::{Cell, UnsafeCell};

// Borrow flag value representing "no active borrow".
const UNUSED: isize = 0;

/// A cheap single-threaded read-write borrow flag for userdata.
///
/// Positive values count active shared borrows; a negative value marks the single active
/// exclusive borrow.
pub struct RawLock(Cell<isize>);

impl RawLock {
    #[inline(always)]
    fn new() -> Self {
        Self(Cell::new(UNUSED))
    }

    #[inline(always)]
    pub(crate) fn is_locked(&self) -> bool {
        self.0.get() != UNUSED
    }

    #[inline(always)]
    fn try_lock_shared(&self) -> bool {
        let flag = self
            .0
            .get()
            .checked_add(1)
            .expect("userdata lock count overflow");
        if flag <= UNUSED {
            return false;
        }
        self.0.set(flag);
        true
    }

    #[inline(always)]
    pub(crate) fn try_lock_exclusive(&self) -> bool {
        if self.0.get() != UNUSED {
            return false;
        }
        self.0.set(UNUSED - 1);
        true
    }

    #[inline(always)]
    fn unlock_shared(&self) {
        let flag = self.0.get();
        debug_assert!(flag > UNUSED);
        self.0.set(flag - 1);
    }

    #[inline(always)]
    fn unlock_exclusive(&self) {
        let flag = self.0.get();
        debug_assert!(flag < UNUSED);
        self.0.set(flag + 1);
    }

    #[inline(always)]
    pub(crate) fn try_lock_shared_guarded(&self) -> Result<LockGuard<'_>, ()> {
        if self.try_lock_shared() {
            Ok(LockGuard {
                lock: self,
                exclusive: false,
            })
        } else {
            Err(())
        }
    }

    #[inline(always)]
    pub(crate) fn try_lock_exclusive_guarded(&self) -> Result<LockGuard<'_>, ()> {
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

/// RAII guard that releases a [`RawLock`] borrow when dropped.
pub struct LockGuard<'a> {
    lock: &'a RawLock,
    exclusive: bool,
}

impl Drop for LockGuard<'_> {
    fn drop(&mut self) {
        if self.exclusive {
            self.lock.unlock_exclusive();
        } else {
            self.lock.unlock_shared();
        }
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
            lock: RawLock::new(),
            data: UnsafeCell::new(value),
        }
    }

    /// Returns a reference to the underlying raw lock.
    #[inline(always)]
    pub(crate) fn raw(&self) -> &RawLock {
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
