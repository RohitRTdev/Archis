use core::mem::size_of;
use alloc::vec::Vec;
use kernel_intf::KError;
use crate::mem;
use crate::sched::ProcessStatus;

pub const INTF_PROCESS_GENERAL_INFO: u32 = 0;
pub const INTF_PROCESS_COMMAND_LINE: u32 = 1;

#[repr(C)]
struct IntfProcessRequest {
    req_type: u32,
    pid: u64,
    buffer: usize,
    bytes_needed: usize,
    bytes_written: usize
}

#[repr(C)]
struct IntfProcessInfo {
    pid: u64,
    ppid: u64,
    pgid: u64,
    sid: u64,
    num_threads: u64,
    status: u8
}

fn handle_process_request(buf: *mut u8) -> Result<(), KError> {
    let req = unsafe { &mut *(buf as *mut IntfProcessRequest) };
    match req.req_type {
        INTF_PROCESS_GENERAL_INFO => handle_general_info(req),
        INTF_PROCESS_COMMAND_LINE => handle_command_line(req),
        _ => Err(KError::InvalidArgument)
    }
}

fn handle_general_info(req: &mut IntfProcessRequest) -> Result<(), KError> {
    let entry_size = size_of::<IntfProcessInfo>();

    if req.bytes_needed == 0 {
        req.bytes_needed = crate::sched::snapshot_all_processes().len() * entry_size;
        req.bytes_written = 0;
        return Ok(());
    }

    if req.bytes_needed % entry_size != 0 {
        return Err(KError::InvalidArgument);
    }

    let snapshot = crate::sched::snapshot_all_processes();
    let required = snapshot.len() * entry_size;

    let entries: Vec<IntfProcessInfo> = snapshot.iter().map(|s| IntfProcessInfo {
        pid: s.id as u64,
        ppid: s.ppid as u64,
        pgid: s.pgid as u64,
        sid: s.sid as u64,
        num_threads: s.num_threads as u64,
        status: match s.status {
            ProcessStatus::Ready => 0,
            ProcessStatus::Suspended => 1,
            ProcessStatus::Terminated => 2
        }
    }).collect();

    let bytes = unsafe { core::slice::from_raw_parts(entries.as_ptr() as *const u8, required) };
    // Round down to a whole entry so userspace never sees a partial IntfProcessInfo.
    let copy_len = required.min(req.bytes_needed) / entry_size * entry_size;
    mem::copy_to_user(req.buffer, bytes.as_ptr(), copy_len)?;
    req.bytes_written = copy_len;
    Ok(())
}

fn handle_command_line(req: &mut IntfProcessRequest) -> Result<(), KError> {
    let process = crate::sched::get_process_info(req.pid as usize).ok_or(KError::NotFound)?;
    let mut joined = process.lock().get_args().join(" ");
    joined.push('\0');

    let required = joined.len();
    if req.bytes_needed == 0 {
        req.bytes_needed = required;
        req.bytes_written = 0;
        return Ok(());
    }

    let copy_len = required.min(req.bytes_needed);
    mem::copy_to_user(req.buffer, joined.as_bytes().as_ptr(), copy_len)?;
    req.bytes_written = copy_len;
    Ok(())
}

pub fn init() {
    crate::intf::register_intf("process", handle_process_request, size_of::<IntfProcessRequest>())
        .expect("Failed to register process interface");
}
