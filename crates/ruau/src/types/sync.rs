#[cfg(feature = "send")]
mod inner {
    use std::sync::{Arc, Weak};

    use parking_lot::{RawMutex, RawThreadId};

    pub type XRc<T> = Arc<T>;
    pub type XWeak<T> = Weak<T>;

    pub type ReentrantMutex<T> = parking_lot::ReentrantMutex<T>;

    pub type ReentrantMutexGuard<'a, T> = parking_lot::ReentrantMutexGuard<'a, T>;

    pub type ArcReentrantMutexGuard<T> =
        parking_lot::lock_api::ArcReentrantMutexGuard<RawMutex, RawThreadId, T>;
}

#[cfg(not(feature = "send"))]
mod inner {
    use std::{
        ops::Deref,
        rc::{Rc, Weak},
    };

    pub type XRc<T> = Rc<T>;
    pub type XWeak<T> = Weak<T>;

    pub struct ReentrantMutex<T>(T);

    impl<T> ReentrantMutex<T> {
        #[inline(always)]
        pub(crate) fn new(val: T) -> Self {
            ReentrantMutex(val)
        }

        #[inline(always)]
        pub(crate) fn lock(&self) -> ReentrantMutexGuard<'_, T> {
            ReentrantMutexGuard(&self.0)
        }

        #[inline(always)]
        pub(crate) fn lock_arc(self: &XRc<Self>) -> ArcReentrantMutexGuard<T> {
            ArcReentrantMutexGuard(Rc::clone(self))
        }

        #[inline(always)]
        pub(crate) fn into_lock_arc(self: XRc<Self>) -> ArcReentrantMutexGuard<T> {
            ArcReentrantMutexGuard(self)
        }

        #[inline(always)]
        pub(crate) fn data_ptr(&self) -> *const T {
            &self.0 as *const _
        }
    }

    pub struct ReentrantMutexGuard<'a, T>(&'a T);

    impl<T> Deref for ReentrantMutexGuard<'_, T> {
        type Target = T;

        #[inline(always)]
        fn deref(&self) -> &Self::Target {
            self.0
        }
    }

    pub struct ArcReentrantMutexGuard<T>(XRc<ReentrantMutex<T>>);

    impl<T> Deref for ArcReentrantMutexGuard<T> {
        type Target = T;

        #[inline(always)]
        fn deref(&self) -> &Self::Target {
            &self.0.0
        }
    }
}

pub use inner::{ArcReentrantMutexGuard, ReentrantMutex, ReentrantMutexGuard, XRc, XWeak};
