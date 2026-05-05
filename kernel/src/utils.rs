use core::{
    cell::UnsafeCell,
    sync::atomic::{AtomicBool, Ordering},
};

use lock_api::{GuardSend, Mutex, RawMutex};
use x86::{
    current::rflags::{self, RFlags},
    irq,
};

pub struct Singleton<T> {
    inner: Mutex<IntLock, Option<T>>,
}

unsafe impl<T: Send> Sync for Singleton<T> {}

impl<T> Singleton<T> {
    pub const fn new() -> Self {
        Self {
            inner: Mutex::const_new(IntLock::INIT, None),
        }
    }

    pub fn install(&self, value: T) {
        let mut guard = self.inner.lock();
        assert!(guard.is_none(), "singleton already installed");
        *guard = Some(value);
    }

    pub fn with<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut T) -> R,
    {
        let mut guard = self.inner.lock();
        f(guard.as_mut().expect("singleton not installed"))
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
        if self
            .locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return false;
        }
        unsafe { *self.saved_if.get() = rflags::read().contains(RFlags::FLAGS_IF) };
        unsafe { irq::disable() };
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

pub fn critical_section<F, R>(f: F) -> R
where
    F: FnOnce() -> R,
{
    let enabled = rflags::read().contains(RFlags::FLAGS_IF);
    unsafe { irq::disable() };
    let res = f();
    if enabled {
        unsafe { irq::enable() };
    }
    res
}
