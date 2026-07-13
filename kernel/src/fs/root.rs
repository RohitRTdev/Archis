use alloc::string::ToString;
use alloc::vec::Vec;
use kernel_intf::driver::{DeviceType, EMPTY_REGION, IrpMajor, IrpMinor, Status};
use kernel_intf::info;

use crate::io::{self, DeviceHandleK};
use crate::sched;

use super::MountSource;
use super::mount;
use super::module_fs;

// Unique partition GUID that build system assigns to the
// root partition (`ROOT_UUID="9ffd2959-915c-479f-8787-1f9f701e1034"`)
// in its on-disk mixed-endian byte encoding.
const ROOT_UUID: [u8; 16] = [
    0x59, 0x29, 0xFD, 0x9F, 0x5C, 0x91, 0x9F, 0x47,
    0x87, 0x87, 0x1F, 0x9F, 0x70, 0x1E, 0x10, 0x34
];

// Devices that report real GPT partition info and carry the designated root
// partition's unique GUID.
fn candidate_partitions() -> Vec<DeviceHandleK> {
    let mut out = Vec::new();
    for dev in io::devices_by_type(DeviceType::Partition) {
        let result = io::io_request_sync(&dev, IrpMajor::Control, IrpMinor::DiskGetPartitionInfo, EMPTY_REGION, 0, None, false);
        let Ok(r) = result else { continue };
        if r.status != Status::Success {
            continue; // Unsupported => this is a raw (non-partition) disk device
        }
        let part_info = unsafe { r.req_info.partition_info };
        if part_info.unique_guid != ROOT_UUID {
            continue; // not the designated root partition
        }
        out.push(dev);
    }
    out
}

pub fn load_root_fs() {
    let candidates = candidate_partitions();
    if candidates.is_empty() {
        info!("load_root_fs: no candidate disk partition found. Continuing with initfs");
        return;
    }

    module_fs::preload_modules();

    // Keep the initfs backend around in case none of the candidates below
    // actually mount -- we can then put it right back at "/".
    let initfs_backend = mount::backend_of("/");

    // Switch root: close process 0's initfs cwd handle first so unmount's
    // busy-check (no open handles anywhere within the mount) can pass, then
    // swap the mount itself, then open + install a handle to the new root.
    sched::clear_init_cwd();
    super::unmount("/").expect("load_root_fs: failed to unmount initfs /");

    for dev in candidates {
        let name = dev.name().to_string();
        match super::mount("/", MountSource::Device(dev)) {
            Ok(()) => {
                let new_root = super::open("/").expect("load_root_fs: failed to open new /");
                sched::set_init_cwd(new_root);
                info!("/ mounted at root disk partition");
                return;
            }
            Err(e) => info!("load_root_fs: mount({}) failed: {:?}", name, e)
        }
    }

    info!("load_root_fs: no candidate partition could be mounted. Continuing with initfs");
    if let Some(backend) = initfs_backend {
        mount::remount("/", backend);
        let cwd = super::open("/").expect("load_root_fs: failed to reopen initfs /");
        sched::set_init_cwd(cwd);
    }
}
