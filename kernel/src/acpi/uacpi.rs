use core::{
    alloc::Layout,
    ffi::{c_char, c_void},
    sync::atomic::{AtomicUsize, Ordering},
};

use alloc::boxed::Box;
use lock_api::RawMutex;
use x86::{
    bits64::rflags::{self, RFlags},
    current::paging::{BASE_PAGE_SIZE, PAddr, PTFlags, VAddr},
    io::{inb, inl, inw, outb, outl, outw},
    irq,
};

use crate::{
    BASE_REVISION,
    acpi::RSDP_REQUEST,
    idt::{clear_handler, install_handler, is_handler_installed},
    mm::{
        align_down, align_up,
        vmm::{self, HHDM_OFFSET, MapError, PageSize},
    },
    print,
    tsc::current_time_ns,
    utils::IntLock,
};

/// Returns the PHYSICAL address of the RSDP structure via *out_rsdp_address.
#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_get_rsdp(out_rsdp_addr: *mut PAddr) -> uacpi_sys::uacpi_status {
    if out_rsdp_addr.is_null() {
        return uacpi_sys::UACPI_STATUS_INVALID_ARGUMENT;
    }
    if let Some(response) = RSDP_REQUEST.response() {
        let hhdm = HHDM_OFFSET.load(Ordering::Relaxed);
        let phys = if let Some(revision) = BASE_REVISION.actual_revision()
            && revision == 3
        {
            response.address as u64
        } else {
            (response.address as u64).wrapping_sub(hhdm)
        };
        unsafe { *out_rsdp_addr = PAddr(phys) };
        uacpi_sys::UACPI_STATUS_OK
    } else {
        uacpi_sys::UACPI_STATUS_NOT_FOUND
    }
}

/// Map a physical memory range starting at 'addr' with length 'len', and return
/// a virtual address that can be used to access it.
///
/// # NOTE:
/// 'addr' may be misaligned, in this case the host is expected to round it
///       down to the nearest page-aligned boundary and map that, while making
///       sure that at least 'len' bytes are still mapped starting at 'addr'. The
///       return value preserves the misaligned offset.
///
///       Example for uacpi_kernel_map(0x1ABC, 0xF00):
///           1. Round down the 'addr' we got to the nearest page boundary.
///              Considering a PAGE_SIZE of 4096 (or 0x1000), 0x1ABC rounded down
///              is 0x1000, offset within the page is 0x1ABC - 0x1000 => 0xABC
///           2. Requested 'len' is 0xF00 bytes, but we just rounded the address
///              down by 0xABC bytes, so add those on top. 0xF00 + 0xABC => 0x19BC
///           3. Round up the final 'len' to the nearest PAGE_SIZE boundary, in
///              this case 0x19BC is 0x2000 bytes (2 pages if PAGE_SIZE is 4096)
///           4. Call the VMM to map the aligned address 0x1000 (from step 1)
///              with length 0x2000 (from step 3). Let's assume the returned
///              virtual address for the mapping is 0xF000.
///           5. Add the original offset within page 0xABC (from step 1) to the
///              resulting virtual address 0xF000 + 0xABC => 0xFABC. Return it
///              to uACPI.
#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_map(addr: PAddr, len: usize) -> *mut c_void {
    let hhdm = HHDM_OFFSET.load(Ordering::Relaxed);
    let page = BASE_PAGE_SIZE as u64;
    let start = align_down(addr.0, page);
    let end = align_up(addr.0 + len as u64, page);
    let flags = PTFlags::P | PTFlags::RW | PTFlags::XD | PTFlags::PCD | PTFlags::PWT;

    let mut phys = start;
    while phys < end {
        match vmm::map(VAddr(phys + hhdm), PAddr(phys), flags, PageSize::Base) {
            Ok(()) | Err(MapError::AlreadyMapped) => {}
            Err(_) => return core::ptr::null_mut(),
        }
        phys += page;
    }

    (addr.0 + hhdm) as _
}

/// Unmap a virtual memory range at 'addr' with a length of 'len' bytes.
///
/// # NOTE:
/// 'addr' may be misaligned, see the comment above 'uacpi_kernel_map'.
///       Similar steps to uacpi_kernel_map can be taken to retrieve the
///       virtual address originally returned by the VMM for this mapping
///       as well as its true length.
#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_unmap(_addr: *mut c_void, _len: usize) {}

/// Log a message at the given level.
#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_log(level: uacpi_sys::uacpi_log_level, msg: *const c_char) {
    if let Ok(s) = unsafe { core::ffi::CStr::from_ptr(msg).to_str() } {
        match level {
            uacpi_sys::UACPI_LOG_DEBUG | uacpi_sys::UACPI_LOG_TRACE => print!("[debug] {s}"),
            uacpi_sys::UACPI_LOG_INFO => print!("[info] {s}"),
            uacpi_sys::UACPI_LOG_WARN => print!("[warning] {s}"),
            uacpi_sys::UACPI_LOG_ERROR => print!("[error] {s}"),
            _ => print!("[unknown log level {level}] {s}"),
        }
    }
}

/// Open a PCI device at 'address' for reading & writing.
///
/// The device at 'address' might not actually exist on the system, in this case
/// the api is allowed to return UACPI_STATUS_NOT_FOUND to indicate that, this
/// error is handled gracefully by creating a dummy device internally that always
/// returns 0xFF on reads and is no-op for writes. This is to support a common
/// pattern in AML that probes for 0xFF reads to detect whether a device exists.
///
/// The handle returned via 'out_handle' is used to perform IO on the
/// configuration space of the device.
#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_pci_device_open(
    address: uacpi_sys::uacpi_pci_address,
    out_handle: *mut uacpi_sys::uacpi_handle,
) -> uacpi_sys::uacpi_status {
    uacpi_sys::UACPI_STATUS_UNIMPLEMENTED
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_pci_device_close(handle: uacpi_sys::uacpi_handle) {}

/// Read the configuration space of a previously open PCI device.
#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_pci_read8(
    device: uacpi_sys::uacpi_handle,
    offset: usize,
    value: *mut u8,
) -> uacpi_sys::uacpi_status {
    uacpi_sys::UACPI_STATUS_UNIMPLEMENTED
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_pci_read16(
    device: uacpi_sys::uacpi_handle,
    offset: usize,
    value: *mut u16,
) -> uacpi_sys::uacpi_status {
    uacpi_sys::UACPI_STATUS_UNIMPLEMENTED
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_pci_read32(
    device: uacpi_sys::uacpi_handle,
    offset: usize,
    value: *mut u32,
) -> uacpi_sys::uacpi_status {
    uacpi_sys::UACPI_STATUS_UNIMPLEMENTED
}

/// Write the configuration space of a previously open PCI device.
#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_pci_write8(
    device: uacpi_sys::uacpi_handle,
    offset: usize,
    value: u8,
) -> uacpi_sys::uacpi_status {
    uacpi_sys::UACPI_STATUS_UNIMPLEMENTED
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_pci_write16(
    device: uacpi_sys::uacpi_handle,
    offset: usize,
    value: u16,
) -> uacpi_sys::uacpi_status {
    uacpi_sys::UACPI_STATUS_UNIMPLEMENTED
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_pci_write32(
    device: uacpi_sys::uacpi_handle,
    offset: usize,
    value: u32,
) -> uacpi_sys::uacpi_status {
    uacpi_sys::UACPI_STATUS_UNIMPLEMENTED
}

/// Map a SystemIO address at [base, base + len) and return a kernel-implemented
/// handle that can be used for reading and writing the IO range.
///
/// # NOTE:
/// The x86 architecture uses the in/out family of instructions
///       to access the SystemIO address space.
#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_io_map(
    base: uacpi_sys::uacpi_io_addr,
    _len: usize,
    out_handle: *mut uacpi_sys::uacpi_handle,
) -> uacpi_sys::uacpi_status {
    unsafe { *out_handle = base as _ };
    uacpi_sys::UACPI_STATUS_OK
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_io_unmap(_handle: uacpi_sys::uacpi_handle) {}

/// Read the IO range mapped via uacpi_kernel_io_map at a 0-based 'offset'
/// within the range.
///
/// # NOTE:
/// The x86 architecture uses the in/out family of instructions
/// to access the SystemIO address space.
///
/// You are NOT allowed to break e.g. a 4-byte access into four 1-byte accesses.
/// Hardware ALWAYS expects accesses to be of the exact width.
#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_io_read8(
    handle: uacpi_sys::uacpi_handle,
    offset: usize,
    out_value: *mut u8,
) -> uacpi_sys::uacpi_status {
    unsafe { *out_value = inb(handle as u16 + offset as u16) };
    uacpi_sys::UACPI_STATUS_OK
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_io_read16(
    handle: uacpi_sys::uacpi_handle,
    offset: usize,
    out_value: *mut u16,
) -> uacpi_sys::uacpi_status {
    unsafe { *out_value = inw(handle as u16 + offset as u16) };
    uacpi_sys::UACPI_STATUS_OK
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_io_read32(
    handle: uacpi_sys::uacpi_handle,
    offset: usize,
    out_value: *mut u32,
) -> uacpi_sys::uacpi_status {
    unsafe { *out_value = inl(handle as u16 + offset as u16) };
    uacpi_sys::UACPI_STATUS_OK
}

/// Write the IO range mapped via uacpi_kernel_io_map at a 0-based 'offset'
/// within the range. See `uacpi_kernel_io_read8` for access-width rules.
#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_io_write8(
    handle: uacpi_sys::uacpi_handle,
    offset: usize,
    in_value: u8,
) -> uacpi_sys::uacpi_status {
    unsafe { outb(handle as u16 + offset as u16, in_value) };
    uacpi_sys::UACPI_STATUS_OK
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_io_write16(
    handle: uacpi_sys::uacpi_handle,
    offset: usize,
    in_value: u16,
) -> uacpi_sys::uacpi_status {
    unsafe { outw(handle as u16 + offset as u16, in_value) };
    uacpi_sys::UACPI_STATUS_OK
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_io_write32(
    handle: uacpi_sys::uacpi_handle,
    offset: usize,
    in_value: u32,
) -> uacpi_sys::uacpi_status {
    unsafe { outl(handle as u16 + offset as u16, in_value) };
    uacpi_sys::UACPI_STATUS_OK
}

/// Allocate a block of memory of 'size' bytes.
/// The contents of the allocated memory are unspecified.
#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_alloc(size: usize) -> *mut c_void {
    Layout::from_size_align(size.max(1), 16).map_or(core::ptr::null_mut(), |layout| unsafe {
        alloc::alloc::alloc(layout) as _
    })
}

/// Free a previously allocated memory block.
///
/// 'mem' might be a NULL pointer. In this case, the call is assumed to be a
/// no-op.
///
/// The 'size_hint' parameter contains the size of the original allocation
/// (enabled via UACPI_SIZED_FREES).
#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_free(mem: *mut c_void, size_hint: usize) {
    if !mem.is_null()
        && let Ok(layout) = Layout::from_size_align(size_hint.max(1), 16)
    {
        unsafe { alloc::alloc::dealloc(mem as *mut u8, layout) };
    }
}

/// Returns the number of nanosecond ticks elapsed since boot,
/// strictly monotonic.
#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_get_nanoseconds_since_boot() -> u64 {
    current_time_ns()
}

/// Spin for N microseconds.
#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_stall(usec: u8) {
    let start = current_time_ns();
    let end = start + usec as u64 * 1000;
    while current_time_ns() < end {}
}

/// Sleep for N milliseconds.
#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_sleep(msec: u64) {
    let start = current_time_ns();
    let end = start + msec * 1000000;
    while current_time_ns() < end {}
}

/// TODO:
/// Create an opaque non-recursive kernel mutex object.
#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_create_mutex() -> uacpi_sys::uacpi_handle {
    uacpi_kernel_create_spinlock()
}

/// Free an opaque non-recursive kernel mutex object.
#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_free_mutex(handle: uacpi_sys::uacpi_handle) {
    uacpi_kernel_free_spinlock(handle);
}

#[derive(Default)]
struct SimpleEvent {
    counter: AtomicUsize,
}

impl SimpleEvent {
    fn decrement(&self) -> bool {
        loop {
            let value = self.counter.load(Ordering::Acquire);
            if value == 0 {
                return false;
            }
            match self.counter.compare_exchange(
                value,
                value - 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(v) if v != 0 => return true,
                Ok(_) => return false,
                Err(_) => continue,
            }
        }
    }
}

/// Create an opaque kernel (semaphore-like) event object.
#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_create_event() -> uacpi_sys::uacpi_handle {
    let event = Box::new(SimpleEvent::default());
    Box::into_raw(event) as _
}

/// Free an opaque kernel (semaphore-like) event object.
#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_free_event(handle: uacpi_sys::uacpi_handle) {
    if !handle.is_null() {
        let _ = unsafe { Box::from_raw(handle as *mut SimpleEvent) };
    }
}

/// Returns a unique identifier of the currently executing thread.
///
/// The returned thread id cannot be UACPI_THREAD_ID_NONE.
#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_get_thread_id() -> uacpi_sys::uacpi_thread_id {
    0 as _
}

/// Disable interrupts and return a kernel-defined value representing the
/// "before" state. This value is used in the subsequent call to restore the
/// prior state.
///
/// Note that this is talking about ALL interrupts on the current CPU, not just
/// those installed by uACPI. This is typically achieved by executing the 'cli'
/// instruction on x86, 'msr daifset, #3' on aarch64 etc.
#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_disable_interrupts() -> uacpi_sys::uacpi_interrupt_state {
    let enabled = rflags::read().contains(RFlags::FLAGS_IF);
    unsafe { irq::disable() };
    enabled as _
}

/// Restore the state of the interrupt flags to the kernel-defined value
/// provided in 'state'.
#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_restore_interrupts(state: uacpi_sys::uacpi_interrupt_state) {
    if state != 0 {
        unsafe { irq::enable() };
    }
}

/// Try to acquire the mutex with a millisecond timeout.
///
/// The timeout value has the following meanings:
/// 0x0000 - Attempt to acquire the mutex once, in a non-blocking manner
/// 0x0001...0xFFFE - Attempt to acquire the mutex for at least 'timeout'
///                   milliseconds
/// 0xFFFF - Infinite wait, block until the mutex is acquired
///
/// The following are possible return values:
/// 1. UACPI_STATUS_OK - successful acquire operation
/// 2. UACPI_STATUS_TIMEOUT - timeout reached while attempting to acquire (or
///    the single attempt to acquire was not successful
///    for calls with timeout=0)
/// 3. Any other value - signifies a host internal error and is treated as such
#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_acquire_mutex(
    handle: uacpi_sys::uacpi_handle,
    timeout: u16,
) -> uacpi_sys::uacpi_status {
    let mutex = unsafe { &*(handle as *const IntLock) };
    let mut locked = false;

    match timeout {
        0xFFFF => {
            mutex.lock();
            return uacpi_sys::UACPI_STATUS_OK;
        }
        0x0000 => locked = mutex.try_lock(),
        _ => {
            let time = current_time_ns();
            while current_time_ns() < time + timeout as u64 * 1_000_000 {
                locked = mutex.try_lock();
                if locked {
                    break;
                }
                uacpi_kernel_sleep(1);
            }
        }
    }

    if locked {
        uacpi_sys::UACPI_STATUS_OK
    } else {
        uacpi_sys::UACPI_STATUS_TIMEOUT
    }
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_release_mutex(handle: uacpi_sys::uacpi_handle) {
    unsafe { (*(handle as *const IntLock)).unlock() };
}

/// Try to wait for an event (counter > 0) with a millisecond timeout.
/// A timeout value of 0xFFFF implies infinite wait.
///
/// The internal counter is decremented by 1 if wait was successful.
///
/// A successful wait is indicated by returning UACPI_TRUE.
#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_wait_for_event(
    handle: uacpi_sys::uacpi_handle,
    timeout: u16,
) -> uacpi_sys::uacpi_bool {
    let event = unsafe { &*(handle as *const SimpleEvent) };
    if timeout == 0xFFFF {
        while !event.decrement() {
            uacpi_kernel_sleep(10);
        }
        true
    } else {
        let mut remaining = timeout as i64;
        while !event.decrement() {
            if remaining <= 0 {
                return false;
            }
            uacpi_kernel_sleep(10);
            remaining -= 10;
        }
        true
    }
}

/// Signal the event object by incrementing its internal counter by 1.
///
/// This function may be used in interrupt contexts.
#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_signal_event(handle: uacpi_sys::uacpi_handle) {
    let event = unsafe { &*(handle as *const SimpleEvent) };
    event.counter.fetch_add(1, Ordering::AcqRel);
}

/// Reset the event counter to 0.
#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_reset_event(handle: uacpi_sys::uacpi_handle) {
    let event = unsafe { &*(handle as *const SimpleEvent) };
    event.counter.store(0, Ordering::Release);
}

/// Handle a firmware request.
///
/// Currently either a Breakpoint or Fatal operators.
#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_handle_firmware_request(
    _req: *mut uacpi_sys::uacpi_firmware_request,
) -> uacpi_sys::uacpi_status {
    uacpi_sys::UACPI_STATUS_UNIMPLEMENTED
}

/// Install an interrupt handler at 'irq', 'ctx' is passed to the provided
/// handler for every invocation.
///
/// 'out_irq_handle' is set to a kernel-implemented value that can be used to
/// refer to this handler from other API.
#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_install_interrupt_handler(
    irq: u32,
    handler: uacpi_sys::uacpi_interrupt_handler,
    ctx: uacpi_sys::uacpi_handle,
    out_irq_handle: *mut uacpi_sys::uacpi_handle,
) -> uacpi_sys::uacpi_status {
    let vector = if cfg!(target_arch = "x86_64") {
        irq + 0x20
    } else {
        irq
    } as u8;

    if is_handler_installed(vector) {
        return uacpi_sys::UACPI_STATUS_ALREADY_EXISTS;
    }

    let ctx = ctx as usize;
    install_handler(vector, move |_| {
        if let Some(handler) = handler.as_ref() {
            unsafe { handler(ctx as _) };
        }
    });

    unsafe { *out_irq_handle = vector as _ };

    uacpi_sys::UACPI_STATUS_OK
}

/// Uninstall an interrupt handler. 'irq_handle' is the value returned via
/// 'out_irq_handle' during installation.
#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_uninstall_interrupt_handler(
    _handler: uacpi_sys::uacpi_interrupt_handler,
    irq_handle: uacpi_sys::uacpi_handle,
) -> uacpi_sys::uacpi_status {
    let vector = irq_handle as u8;

    clear_handler(vector);

    uacpi_sys::UACPI_STATUS_OK
}

/// Create a kernel spinlock object.
///
/// Unlike other types of locks, spinlocks may be used in interrupt contexts.
#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_create_spinlock() -> uacpi_sys::uacpi_handle {
    let lock = Box::new(IntLock::INIT);
    Box::into_raw(lock) as _
}

/// Free a kernel spinlock object.
#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_free_spinlock(handle: uacpi_sys::uacpi_handle) {
    if !handle.is_null() {
        let _ = unsafe { Box::from_raw(handle as *mut IntLock) };
    }
}

/// Lock a spinlock.
///
/// Expected to disable interrupts, returning the previous state of cpu flags,
/// that can be used to possibly re-enable interrupts if they were enabled
/// before.
///
/// Note that lock is infalliable.
#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_lock_spinlock(
    handle: uacpi_sys::uacpi_handle,
) -> uacpi_sys::uacpi_cpu_flags {
    if handle.is_null() {
        return 0;
    }
    let lock = unsafe { &*(handle as *const IntLock) };
    lock.lock();
    0
}

/// Unlock a spinlock, restoring the previous cpu flags state.
#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_unlock_spinlock(
    handle: uacpi_sys::uacpi_handle,
    _flags: uacpi_sys::uacpi_cpu_flags,
) {
    if handle.is_null() {
        return;
    }
    let lock = unsafe { &*(handle as *const IntLock) };
    unsafe { lock.unlock() };
}

/// Schedules deferred work for execution.
/// Might be invoked from an interrupt context.
#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_schedule_work(
    _work_type: uacpi_sys::uacpi_work_type,
    _handler: uacpi_sys::uacpi_work_handler,
    _ctx: uacpi_sys::uacpi_handle,
) -> uacpi_sys::uacpi_status {
    uacpi_sys::UACPI_STATUS_UNIMPLEMENTED
}

/// Waits for two types of work to finish:
/// 1. All in-flight interrupts installed via
///    uacpi_kernel_install_interrupt_handler
/// 2. All work scheduled via uacpi_kernel_schedule_work
///
/// Note that the waits must be done in this order specifically.
#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_wait_for_work_completion() -> uacpi_sys::uacpi_status {
    uacpi_sys::UACPI_STATUS_UNIMPLEMENTED
}
