use super::asm;
use kernel_intf::debug;
use crate::hal;


static mut COPY_USER_FN_PTR: unsafe fn(to: *mut u8, from: *const u8, len: usize) = do_copy_basic;
static mut SET_USER_FN_PTR: unsafe fn(to: *mut u8, value: u8, len: usize) = do_set_basic;

#[unsafe(no_mangle)]
pub extern "C" fn read_timestamp() -> usize {
    asm::rdtsc() as usize
}


#[derive(Debug, Clone, Copy)]
pub struct VirtAddr(u64);

impl VirtAddr {
    #[cfg(not(test))]
    #[inline(always)]
    pub fn new(addr: usize) -> Self {
        // Virtual address in AMD/Intel for 64 bit mode is 48 bits. All upper bits must match 47th bit
        Self (Self::canonicalize(addr as u64, 47))
    }
    
    #[cfg(test)]
    pub fn new(addr: usize) -> Self {
        Self (Self::canonicalize(addr as u64, 47))
    }

    #[inline(always)]
    fn canonicalize(mut addr: u64, last_bit: u8) -> u64 {
        if addr & (1 << last_bit) != 0 {
            addr |= (0xffff as u64) << (last_bit + 1);
        }
        else {
            addr &= !((0xffff as u64) << (last_bit + 1));
        }

        addr
    }

    #[inline(always)]
    pub fn get(&self) -> usize {
        self.0 as usize
    }
}

pub fn canonicalize_virtual(addr: usize) -> usize {
    VirtAddr::new(addr).get()
}

pub(super) fn enable_smap_feature() {
    unsafe {
        COPY_USER_FN_PTR = do_copy_smap;
        SET_USER_FN_PTR = do_set_smap;
    }
}

unsafe fn do_copy_basic(to: *mut u8, from: *const u8, len: usize) {
    unsafe {
        core::ptr::copy_nonoverlapping(from, to, len);
    }
}

unsafe fn do_copy_smap(to: *mut u8, from: *const u8, len: usize) {
    unsafe {
        core::arch::asm!(
            "cld",    
            "stac",   
            "rep movsb", 
            "clac",      
            inout("rdi") to => _,
            inout("rsi") from => _,
            inout("rcx") len => _,
            options(nostack)
        );
    }
}


pub unsafe fn copy_user_memory(to: *mut u8, from: *const u8, len: usize) {
    if len == 0 {
        return;
    }

    unsafe {
        COPY_USER_FN_PTR(to, from, len);
    }
}

unsafe fn do_set_basic(to: *mut u8, value: u8, len: usize) {
    unsafe {
        to.write_bytes(value, len);
    }
}

unsafe fn do_set_smap(to: *mut u8, value: u8, len: usize) {
    unsafe {
        core::arch::asm!(
            "cld",
            "stac",
            "rep stosb",
            "clac",
            inout("rdi") to => _,
            inout("al") value => _,
            inout("rcx") len => _,
            options(nostack)
        ); 
    }
}

pub unsafe fn set_user_memory(to: *mut u8, value: u8, len: usize) {
    if len == 0 {
        return;
    }

    unsafe {
        SET_USER_FN_PTR(to, value, len);
    }
}

pub fn switch_to_new_address_space(pml4_phys: usize, stack_address: usize, kernel_address: usize) -> ! {
    debug!("kern_address_space_start address = {:#X}", kernel_address);

    // Special hook to tell logger to update its internal pointers now
    crate::logger::relocate_framebuffer();
     
    unsafe {
        asm::init_address_space(pml4_phys as u64, stack_address as u64,  kernel_address as u64);
        
        // Shouldn't reach here
        hal::halt()
    }
}