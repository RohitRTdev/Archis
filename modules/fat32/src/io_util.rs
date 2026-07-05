use kernel_intf::E_INTERNAL_FAILURE;
use kernel_intf::driver::{DeviceObject, DiskInfo, IrpMajor, IrpMinor, Status};

pub const SECTOR_SIZE: usize = 512;

pub fn read_sectors(dev: *const DeviceObject, lba: u64, buf: &mut [u8]) -> Result<(), i64> {
    assert!(buf.len() % SECTOR_SIZE == 0 && !buf.is_empty());

    // Disk driver expects offset to be in lba and buf to 
    // length that is multiple of SECTOR_SIZE
    let result = kernel_intf::io_send_request(
        dev,
        IrpMajor::Read as usize,
        IrpMinor::None as usize,
        buf.as_mut_ptr() as usize,
        buf.len(),
        lba as usize,
        core::ptr::null(),
        None,
        core::ptr::null_mut()
    );

    // Internal failure could be something like, device abruptly stopped
    if result.status == Status::Success { Ok(()) } else { Err(E_INTERNAL_FAILURE) }
}

pub fn write_sectors(dev: *const DeviceObject, lba: u64, buf: &[u8]) -> Result<(), i64> {
    debug_assert!(buf.len() % SECTOR_SIZE == 0 && !buf.is_empty());
    let result = kernel_intf::io_send_request(
        dev,
        IrpMajor::Write as usize,
        IrpMinor::None as usize,
        buf.as_ptr() as usize,
        buf.len(),
        lba as usize,
        core::ptr::null(),
        None,
        core::ptr::null_mut()
    );
    if result.status == Status::Success { Ok(()) } else { Err(E_INTERNAL_FAILURE) }
}

// Queries DiskGetInfo (works on raw or partition devices — see disk driver).
// The driver writes its answer into req_info, which travels back in the
// completed IrpResult (io_send_request now returns the whole thing, not
// just a Status).
pub fn get_disk_info(dev: *const DeviceObject) -> Result<DiskInfo, i64> {
    let result = kernel_intf::io_send_request(
        dev,
        IrpMajor::Control as usize,
        IrpMinor::DiskGetInfo as usize,
        0,
        0,
        0,
        core::ptr::null(),
        None,
        core::ptr::null_mut()
    );
    if result.status == Status::Success {
        Ok(unsafe { result.req_info.disk_info })
    } else {
        Err(E_INTERNAL_FAILURE)
    }
}
