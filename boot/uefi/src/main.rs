#![cfg_attr(not(test), no_std)]
#![no_main]

mod loader;
mod logger;
mod display;

use common::{ArrayTable, BootInfo, MemType, MemoryDesc, MemoryRegion, FileDescriptor, MAX_DESCRIPTORS, PAGE_SIZE};
use uefi::{mem::memory_map::MemoryMap, prelude::*};
use uefi::boot::{MemoryAttribute, MemoryType};
use log::{info, debug};
use core::mem;

#[cfg(not(test))]
use core::panic::PanicInfo;

use core::alloc::Layout;
use uefi::{Identify, proto::media::fs::SimpleFileSystem};
use blr::{KERNEL_FILE, INITFS_CONF, load_kernel, jump_to_kernel};

extern crate alloc;

#[unsafe(no_mangle)]
extern "Rust" fn loader_alloc(layout: Layout) -> *mut u8 {
    assert!(layout.align() <= PAGE_SIZE, "Cannot satisfy memory alignment constraint of more than 4096 bytes!!");
    debug!("Requesting memory allocation for {:?}", layout);

    let pages = common::ceil_div(layout.size(), PAGE_SIZE);

    boot::allocate_pages(boot::AllocateType::AnyPages, MemoryType::LOADER_DATA, pages).expect(
        "Memory allocation failed!!"
    ).as_ptr() 
}


fn setup_memory_map() -> ArrayTable {
    let layout = Layout::array::<MemoryDesc>(MAX_DESCRIPTORS).unwrap();
    let base = unsafe {
        core::slice::from_raw_parts_mut(loader_alloc(layout) as *mut MemoryDesc, MAX_DESCRIPTORS)
    };

    info!("Exiting boot services!");
    let memmap = unsafe {
        boot::exit_boot_services(Some(MemoryType::LOADER_DATA))
    };
    
    let total_entries = memmap.len().min(MAX_DESCRIPTORS);

    // Classify memory as free, allocated or runtime
    // runtime means this memory location is used by firmware and it needs to be identity mapped by aris later
    for (idx, desc) in memmap.entries().enumerate() {
        if idx >= MAX_DESCRIPTORS {
            break;
        }

        let mem_type = match desc.ty {
                MemoryType::BOOT_SERVICES_CODE |
                MemoryType::CONVENTIONAL | MemoryType::PERSISTENT_MEMORY => {
                    MemType::Free
                },
                MemoryType::RUNTIME_SERVICES_CODE | MemoryType::RUNTIME_SERVICES_DATA
                | MemoryType::ACPI_NON_VOLATILE | MemoryType::ACPI_RECLAIM => {
                    MemType::Identity
                },
                MemoryType::BOOT_SERVICES_DATA => {
                    MemType::BootloaderData
                },
                _ => {
                    MemType::Allocated
                }
            };

        base[idx] = MemoryDesc {
            val: MemoryRegion {
                base_address: desc.phys_start as usize,
                size: desc.page_count as usize * PAGE_SIZE as usize
            },
            mem_type: if desc.att.contains(MemoryAttribute::RUNTIME) {
                    MemType::Identity
                }
                else {
                    mem_type
                }
            };
    }

    // If memmap gets dropped, it will call UEFI allocator to free memory which would crash system
    mem::forget(memmap);
    
    ArrayTable {start: base.as_ptr() as usize, size: total_entries * size_of::<MemoryDesc>(), entry_size: size_of::<MemoryDesc>()}
}

#[cfg(feature = "acpi")]
fn get_rsdp() -> usize {
    const ACPI_GUID: &str = "8868e871-e4f1-11d3-bc22-0080c73c8881";  

    let tab = uefi::table::system_table_raw().unwrap();    
    let config = unsafe {
        core::slice::from_raw_parts(tab.as_ref().configuration_table, tab.as_ref().number_of_configuration_table_entries)
    };

    let mut rsdp = None;
    for table in config {
        let ascii_data = table.vendor_guid.to_ascii_hex_lower();
        if str::from_utf8(&ascii_data).unwrap() == ACPI_GUID {
            info!("Found ACPI rev:2.0 table at address:{:#X}", table.vendor_table as usize);
            rsdp = Some(table.vendor_table as usize);
            break;
        }
    }

    if rsdp.is_none() {
        panic!("No ACPI-2.0 table found!! Cannot proceed..");
    }

    rsdp.unwrap() 
}


#[entry]
fn main() -> Status {
    logger::init_logger();
    
    // First get all available handles for Simple filesystem protocol
    info!("Fetching FAT32 formatted partitions...");
    let supported_handles = boot::locate_handle_buffer(boot::SearchType::ByProtocol(&SimpleFileSystem::GUID)).expect("Unable to locate partitions with fat32 fs");

    let root_partition = loader::list_fs(&supported_handles);
    let file_table = loader::load_init_fs(root_partition, INITFS_CONF);
    
    let kernel_data = file_table.fetch_file_data(KERNEL_FILE).unwrap();
    let kern_info  = load_kernel(kernel_data.as_ptr());

    debug!("{:?}", kern_info);

#[cfg(feature = "acpi")]
    let rsdp = get_rsdp();

    info!("Fetching GPU and memmap info before transferring control to aris");
    let fb_info = display::get_primary_gpu_framebuffer();
    let mem_info = setup_memory_map();
    let fs_info = ArrayTable {start: file_table.descriptors.as_ptr() as usize, 
        size: size_of::<FileDescriptor>() * file_table.length, entry_size: size_of::<FileDescriptor>()};

    let boot_info = BootInfo {kernel_desc: kern_info, framebuffer_desc: fb_info, memory_map_desc: mem_info, init_fs: fs_info,
#[cfg(feature = "acpi")]
        rsdp
    };

    unsafe {
        jump_to_kernel(&boot_info);
    }
}


#[cfg(not(test))]
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    system::with_stdout(|output| {
        output.clear().unwrap();
    });

    println!("[PANIC!!!]: {}", info.message());
    loop{}
}