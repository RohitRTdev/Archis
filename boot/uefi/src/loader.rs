extern crate alloc;

use core::ptr::copy_nonoverlapping;
use core::alloc::Layout;
use core::str::from_utf8;
use alloc::format;
use blr::{INITFS_CONF, KERNEL_FILE};
use log::info;
use alloc::vec;
use alloc::{string::String, vec::Vec};
use alloc::borrow::ToOwned;
use uefi::proto::device_path::text::{AllowShortcuts, DisplayOnly};
use uefi::proto::media::file::{Directory, File, FileAttribute, RegularFile};
use uefi::proto::loaded_image::LoadedImage;
use uefi::{boot, CString16, Char16, Handle};
use uefi::boot::{ScopedProtocol, HandleBuffer};
use uefi::proto::{device_path::{DevicePath, media::{self,HardDrive}}, media::{file::FileMode, fs::SimpleFileSystem}};
use common::{FileDescriptor, PAGE_SIZE};
use crate::loader_alloc;

const ROOT_GUID: &str = "9ffd2959-915c-479f-8787-1f9f701e1034";  


pub struct FileTable {
    pub descriptors: &'static mut [FileDescriptor],
    capacity: usize,
    pub length: usize
}

impl FileTable {
    fn new() -> Self {
        // Let's start with a backing memory of 1 page size
        // We're not simply creating a vector here as we need precise control over alignment of memory
        let init_cap = PAGE_SIZE;
        let length = init_cap / size_of::<FileDescriptor>();
        let layout = Layout::from_size_align(init_cap, PAGE_SIZE).unwrap();
        Self {
            descriptors: unsafe {
                core::slice::from_raw_parts_mut(loader_alloc(layout) as *mut FileDescriptor, length)
            },
            capacity: init_cap,
            length: 0
        }
    }

    fn insert(&mut self, name: &str, value: &[u8]) {
        let layout = Layout::from_size_align(name.len() + value.len(), PAGE_SIZE).unwrap();
        let loc = loader_alloc(layout);

        // Copy the file contents first before name
        // This ensures elf file is 4K aligned. String can be arbitrary alignment
        unsafe {
            copy_nonoverlapping(value.as_ptr(), loc, value.len());
            copy_nonoverlapping(name.as_ptr(), loc.add(value.len()), name.len());
        }

        let desc = FileDescriptor {
            name: unsafe {
                core::str::from_utf8(core::slice::from_raw_parts(loc.add(value.len()), name.len())).unwrap()
            },
            contents: unsafe {
                core::slice::from_raw_parts(loc, value.len())
            }
        };

        // TODO: If not enough capacity, then allocate bigger array and copy old contents to it
        // For now, this should suffice
        assert!(self.length < self.capacity / size_of::<FileDescriptor>());
        self.descriptors[self.length] = desc;
        self.length += 1;
    } 
    
    pub fn fetch_file_data(&self, filename: &str) -> Option<&[u8]> {
        for desc in self.descriptors.iter() {
            if desc.name == filename {
                return Some(desc.contents)
            }
        }
    
        None
    }
}


// Collect the raw bytes of all device path nodes that precede the HardDrive node.
// This identifies the physical disk controller path, independent of partition number.
fn disk_prefix_bytes(device_path: &DevicePath) -> Vec<u8> {
    let mut bytes = Vec::new();
    for node in device_path.node_iter() {
        if <&HardDrive>::try_from(node).is_ok() {
            break;
        }
        // EFI DevicePath node layout: [type(1), subtype(1), len_lo(1), len_hi(1), data...]
        let node_ptr = node as *const _ as *const u8;
        let len = unsafe {
            u16::from_le_bytes([*node_ptr.add(2), *node_ptr.add(3)]) as usize
        };
        unsafe { bytes.extend_from_slice(core::slice::from_raw_parts(node_ptr, len)); }
    }
    bytes
}

pub fn list_fs(supported_handles: &HandleBuffer) -> &Handle {
    // Determine which physical disk we booted from so that partitions on
    // unrelated disks (even with the same GPT GUID) are ignored.
    let boot_prefix = {
        let image: ScopedProtocol<LoadedImage> =
            boot::open_protocol_exclusive(boot::image_handle()).unwrap();
        let boot_device = image.device().unwrap();
        let boot_path: ScopedProtocol<DevicePath> =
            boot::open_protocol_exclusive(boot_device).unwrap();
        disk_prefix_bytes(&boot_path)
    };
    info!("Boot disk prefix: {} bytes", boot_prefix.len());

    let mut root_partition = None;
    for partition in supported_handles.iter() {
        let device_path: ScopedProtocol<DevicePath> = boot::open_protocol_exclusive(*partition).unwrap();

        if let Ok(device_path_text) = device_path.to_string(DisplayOnly(false), AllowShortcuts(true)) {
            info!("Device path = {}", String::from(&device_path_text));
        }

        // Skip partitions that do not reside on the boot disk
        if disk_prefix_bytes(&device_path) != boot_prefix {
            continue;
        }

        // Iterate the device path and check for our root partition id
        for device_node in device_path.node_iter() {
            if let Ok(device_node_data) = <&HardDrive>::try_from(device_node) {
                if let media::PartitionSignature::Guid(guid) = device_node_data.partition_signature() {
                    let ascii_data = guid.to_ascii_hex_lower();
                    if str::from_utf8(&ascii_data).unwrap() == ROOT_GUID {
                        info!("Found root partition with guid:{}", ROOT_GUID);
                        root_partition = Some(partition);
                        break;
                    }
                }
            }
        }
    }

    root_partition.unwrap_or_else(|| panic!("Could not find root partition!!"))
}

fn parse_initfs_conf(init_conf: &str) -> Vec<String> {
    let mut list: Vec<String> = init_conf
        .split("\n")
        .map(|t| t.trim().to_owned())
        .filter(|t| !t.is_empty())
        .collect();
    info!("Found {} entries in {}", list.len(), INITFS_CONF);
    list.insert(0, KERNEL_FILE.to_owned());

    list
}

fn read_file(dir: &mut Directory, filename: &str) -> Vec<u8> {
    let tmp_name = filename.strip_prefix('/');
    let filename_new = if tmp_name.is_none() {
        filename
    }
    else {
        tmp_name.unwrap()
    };

    let mut filename_dos = CString16::try_from(filename_new).unwrap();
    filename_dos.replace_char(Char16::try_from('/').unwrap(), Char16::try_from('\\').unwrap());

    let file = dir.open(&filename_dos, FileMode::Read, FileAttribute::READ_ONLY).
    expect(format!("Could not open file={}", filename).as_str());
    
    let mut reg_file = file.into_regular_file().unwrap();
    reg_file.set_position(RegularFile::END_OF_FILE).unwrap();
    let file_size = reg_file.get_position().unwrap();
    reg_file.set_position(0).unwrap();

    let mut buf: Vec<u8> = vec![0; file_size as usize];
    reg_file.read(buf.as_mut_slice()).unwrap();


    buf
}

pub fn load_init_fs(root: &Handle, init_conf: &str) -> FileTable {
    // Load all boot start drivers, font file, kernel elf etc
    let mut boot_fs: ScopedProtocol<SimpleFileSystem> = boot::open_protocol_exclusive(*root).expect("Could not open file protocol on root partition"); 

    let mut dir = boot_fs.open_volume().expect("Could not open root partition");
    let initfs = read_file(&mut dir, init_conf);  
    let initfs_contents = from_utf8(initfs.as_slice()).expect("Init fs conf not in UTF-8 format!");
    let files =  parse_initfs_conf(initfs_contents); 

    let mut map = FileTable::new();
    for filename in files {
        let file_contents = read_file(&mut dir, filename.as_str());
        
        info!("Loaded file={} of size={} at location={:#X}", filename, file_contents.len(), file_contents.as_ptr().addr());
        map.insert(filename.as_str(), file_contents.as_slice());
    }

    map
}