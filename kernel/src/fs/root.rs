use kernel_intf::driver::{CreatePartitionInfo, DeviceType, EMPTY_REGION, IrpMajor, IrpMinor, ReqInfo, Status};
use kernel_intf::info;

use crate::io::{self, DeviceHandleK};
use crate::loader::{LoadedImage, load_image};
use crate::sched;
use crate::ROOT_UUID;

use super::MountSource;
use super::mount;

const FAT32_MODULE_PATH: &str = "/sys/libfat32.so";

const GENERIC_DATA_TYPE_GUID: [u8; 16] = [
    0x0F, 0xC6, 0x3D, 0xAF, 0x84, 0x83, 0x47, 0x72,
    0x8E, 0x79, 0x3D, 0x69, 0xD8, 0x47, 0x7D, 0xE4
];

fn encode_utf16_fixed(s: &str) -> [u16; 36] {
    let mut out = [0u16; 36];
    for (i, c) in s.encode_utf16().enumerate() {
        if i >= out.len() {
            break;
        }
        out[i] = c;
    }
    out
}

// Creates a fresh GPT on disk0 with a single partition tagged ROOT_UUID,
// and formats it as FAT32
fn bootstrap_test_disk() -> LoadedImage {
    let disk0 = io::open_device_handle("disk0").expect("load_root_fs: disk0 device not found");

    let r = io::io_request_sync(&disk0, IrpMajor::Control, IrpMinor::DiskCreateGpt, EMPTY_REGION, 0, None, false)
        .expect("load_root_fs: DiskCreateGpt request failed");
    assert!(r.status == Status::Success, "load_root_fs: DiskCreateGpt rejected");

    let r = io::io_request_sync(&disk0, IrpMajor::Control, IrpMinor::DiskGetInfo, EMPTY_REGION, 0, None, false)
        .expect("load_root_fs: DiskGetInfo request failed");
    assert!(r.status == Status::Success, "load_root_fs: DiskGetInfo rejected");
    let disk_info = unsafe { r.req_info.disk_info };

    let start_lba: u64 = 2048;
    let num_lba = disk_info.lba_count.saturating_sub(start_lba).saturating_sub(100);
    let create_info = CreatePartitionInfo {
        part_type_guid: GENERIC_DATA_TYPE_GUID,
        unique_guid: ROOT_UUID,
        start_lba,
        num_lba,
        name_utf16: encode_utf16_fixed("ARCHIS")
    };
    let r = io::io_request_sync(
        &disk0, IrpMajor::Control, IrpMinor::DiskAddPartition, EMPTY_REGION, 0,
        Some(ReqInfo { create_partition: create_info }), false
    ).expect("load_root_fs: DiskAddPartition request failed");
    assert!(r.status == Status::Success, "load_root_fs: DiskAddPartition rejected");

    // The disk driver creates + starts disk0p0 as a side effect of AddPartition.
    let part = io::open_device_handle("disk0p0").expect("load_root_fs: disk0p0 device not found after AddPartition");

    let image = load_image(FAT32_MODULE_PATH, false, true).expect("load_root_fs: failed to load fat32 module");
    let format_addr = image.lock().load_symbol("format").expect("load_root_fs: fat32 module has no 'format' export");
    let format_fn: extern "C" fn(*const kernel_intf::driver::DeviceObject) -> i64 = unsafe { core::mem::transmute(format_addr) };
    assert!(!mount::is_device_mounted(part.device_ptr()), "load_root_fs: refusing to format an already-mounted device");
    let code = format_fn(part.device_ptr());
    assert!(code == kernel_intf::E_SUCCESS, "load_root_fs: fat32 format() failed with code {}", code);

    info!("load_root_fs: bootstrapped GPT + FAT32 on disk0p0 (ROOT_UUID)");
    image
}

fn find_root_partition() -> Option<DeviceHandleK> {
    for dev in io::devices_by_type(DeviceType::Disk) {
        let result = io::io_request_sync(&dev, IrpMajor::Control, IrpMinor::DiskGetPartitionInfo, EMPTY_REGION, 0, None, false);
        let Ok(r) = result else { continue };
        if r.status != Status::Success {
            continue;
        }
        let part_info = unsafe { r.req_info.partition_info };
        if part_info.unique_guid == ROOT_UUID {
            return Some(dev);
        }
    }
    None
}

pub fn load_root_fs() {
    // Keep a strong reference to the all loaded filesystem modules 
    // Currently its just fat32
    let _fat32_module = bootstrap_test_disk();

    let root_dev = find_root_partition();
    if root_dev.is_none() {
        info!("Did not find root partition. Continuing with initfs");
        return;
    }

    let root_dev = root_dev.unwrap();
    // Switch root: close process 0's initfs cwd handle first so unmount's
    // busy-check (no open handles anywhere within the mount) can pass, then
    // swap the mount itself, then open + install a handle to the new root.
    sched::clear_init_cwd();
    super::unmount("/").expect("load_root_fs: failed to unmount initfs /");
    super::mount("/", MountSource::Device(root_dev)).expect("load_root_fs: no fs module identified the root partition");
    let new_root = super::open("/").expect("load_root_fs: failed to open new /");
    sched::set_init_cwd(new_root);

    info!("/ mounted at root disk partition");
}
