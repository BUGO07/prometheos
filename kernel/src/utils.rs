use core::cell::RefCell;

use x86::bits64::rflags::RFlags;

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

pub fn int_free<R>(f: impl FnOnce() -> R) -> R {
    let enabled = x86::current::rflags::read().contains(RFlags::FLAGS_IF);
    unsafe { x86::irq::disable() };
    let res = f();
    if enabled {
        unsafe { x86::irq::enable() };
    }
    res
}
