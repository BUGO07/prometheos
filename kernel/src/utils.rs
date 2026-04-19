use core::{
    cell::{RefCell, UnsafeCell},
    sync::atomic::{AtomicBool, Ordering},
};

use lock_api::{GuardSend, RawMutex};
use x86::{
    current::rflags::{self, RFlags},
    irq,
};

pub struct Singleton<T> {
    inner: RefCell<Option<T>>,
}

unsafe impl<T: Send> Sync for Singleton<T> {}

impl<T> Singleton<T> {
    pub const fn new() -> Self {
        Self {
            inner: RefCell::new(None),
        }
    }

    pub fn install(&self, value: T) {
        int_free(|| {
            assert!(self.inner.borrow().is_none(), "Singleton already installed");
            *self.inner.borrow_mut() = Some(value);
        });
    }

    pub fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        int_free(|| {
            f(self
                .inner
                .borrow_mut()
                .as_mut()
                .expect("Singleton not installed"))
        })
    }

    /// # Safety
    /// Caller must guarantee no concurrent access to the singleton.
    pub unsafe fn with_unchecked<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        let opt = unsafe { &mut *self.inner.as_ptr() };
        f(opt.as_mut().expect("Singleton not installed"))
    }
}

pub struct IntLock {
    locked: AtomicBool,
    saved_if: UnsafeCell<bool>,
}

unsafe impl Sync for IntLock {}

unsafe impl RawMutex for IntLock {
    const INIT: Self = Self {
        locked: AtomicBool::new(false),
        saved_if: UnsafeCell::new(false),
    };

    type GuardMarker = GuardSend;

    fn lock(&self) {
        while !self.try_lock() {
            core::hint::spin_loop();
        }
    }

    fn try_lock(&self) -> bool {
        let was_enabled = rflags::read().contains(RFlags::FLAGS_IF);
        unsafe { irq::disable() };
        if self.locked.swap(true, Ordering::Acquire) {
            if was_enabled {
                unsafe { irq::enable() };
            }
            return false;
        }
        unsafe { *self.saved_if.get() = was_enabled };
        true
    }

    unsafe fn unlock(&self) {
        let restore = unsafe { *self.saved_if.get() };
        self.locked.store(false, Ordering::Release);
        if restore {
            unsafe { irq::enable() };
        }
    }

    fn is_locked(&self) -> bool {
        self.locked.load(Ordering::Relaxed)
    }
}

pub fn int_free<R>(f: impl FnOnce() -> R) -> R {
    let enabled = rflags::read().contains(RFlags::FLAGS_IF);
    unsafe { irq::disable() };
    let res = f();
    if enabled {
        unsafe { irq::enable() };
    }
    res
}
