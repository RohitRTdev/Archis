#![cfg_attr(not(test), no_std)]

#[cfg(test)]
mod tests;
mod arch;

use common::{elf::*, *};
use arch::*;

pub const KERNEL_FILE: &str = "/sys/aris";
pub const INITFS_CONF: &str = "/sys/initfs.conf";

pub unsafe fn jump_to_kernel(boot_info: &BootInfo) -> ! {
    let kern_addr = canonicalize(boot_info.kernel_desc.entry);
    let kern_fn: extern "sysv64" fn(*const BootInfo) -> ! = unsafe {
        core::mem::transmute(kern_addr)
    };

    kern_fn(boot_info as *const BootInfo)
}


pub fn load_kernel(kernel_file: *const u8) -> ModuleInfo {
    let signature = unsafe {
        *(kernel_file as *const u32)
    };

    if signature != ELFMAG {
        panic!("Invalid signature for kernel elf file = {}!", signature);
    }

    let elf_hdr = unsafe {
        &*(kernel_file as *const Elf64Ehdr)
    };


    test_log!("{:?}", elf_hdr);

    load_kernel_arch(kernel_file, elf_hdr)
}

