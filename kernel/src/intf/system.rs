use core::mem::size_of;
use alloc::vec::Vec;
use kernel_intf::KError;
use crate::mem;

pub const INTF_SYSTEM_KLOG: u32 = 0;

#[repr(C)]
struct IntfSystemRequest {
    req_type: u32,
    buffer: usize,
    bytes_needed: usize,
    bytes_written: usize
}

fn handle_system_request(buf: *mut u8) -> Result<(), KError> {
    let req = unsafe { &mut *(buf as *mut IntfSystemRequest) };
    match req.req_type {
        INTF_SYSTEM_KLOG => handle_klog(req),
        _ => Err(KError::InvalidArgument)
    }
}

fn handle_klog(req: &mut IntfSystemRequest) -> Result<(), KError> {
    let mut total: Vec<u8> = Vec::new();
    crate::logger::kring::kring_log_for_each(|s| total.extend_from_slice(s.as_bytes()));

    if req.bytes_needed == 0 {
        req.bytes_needed = total.len();
        req.bytes_written = 0;
        return Ok(());
    }

    let copy_len = total.len().min(req.bytes_needed);
    mem::copy_to_user(req.buffer, total.as_ptr(), copy_len)?;
    req.bytes_written = copy_len;
    Ok(())
}

pub fn init() {
    crate::intf::register_intf("system", handle_system_request, size_of::<IntfSystemRequest>())
        .expect("Failed to register system interface");
}
