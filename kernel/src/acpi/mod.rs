use core::ffi::CStr;

use limine::request::RsdpRequest;
use uacpi_sys::{
    uacpi_finalize_gpe_initialization, uacpi_initialize, uacpi_namespace_initialize,
    uacpi_namespace_load, uacpi_status_to_string,
};

use crate::println;

pub mod uacpi;

#[used]
#[unsafe(link_section = ".requests")]
static RSDP_REQUEST: RsdpRequest = RsdpRequest::new();

pub fn status_to_result(status: uacpi_sys::uacpi_status) -> Result<(), &'static str> {
    if status != uacpi_sys::UACPI_STATUS_OK {
        Ok(())
    } else {
        Err(unsafe {
            CStr::from_ptr(uacpi_status_to_string(status))
                .to_str()
                .unwrap_or("Unknown error")
        })
    }
}

pub fn init() -> Result<(), &'static str> {
    println!("init");
    status_to_result(unsafe { uacpi_initialize(0) })?;
    status_to_result(unsafe { uacpi_namespace_load() })?;
    status_to_result(unsafe { uacpi_namespace_initialize() })?;
    status_to_result(unsafe { uacpi_finalize_gpe_initialization() })?;
    println!("done");
    Ok(())
}
