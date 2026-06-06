use core::alloc::Layout;
use core::sync::atomic::{Ordering, AtomicUsize};
use core::hint::unlikely;
use core::ptr::copy_nonoverlapping;
use crate::cpu;
use crate::{hal::x86_64::features::CPU_FEATURES, mem};
use crate::hal::{VirtAddr, notify_core};
use kernel_intf::info;
use common::{MemoryRegion, PAGE_SIZE, ceil_div, en_flag, usize_to_ptr};
use super::asm;
use super::IPIRequestType;

struct PTE;

impl PTE {
    pub const P: u64 = 1;
    pub const RW: u64 = 1 << 1;
    pub const U: u64 = 1 << 2;
    pub const PWT: u64 = 1 << 3;
    pub const PCD: u64 = 1 << 4;
    pub const G: u64 = 1 << 8;
    pub const PAT: u64 = 1 << 7;
    pub const PHY_ADDR_MASK: u64 = 0x000fffff_fffff000;
}

#[derive(Debug, Clone, Copy)]
enum PageLevel {
    PML4,
    PDPT,
    PD,
    PT
}

static KERNEL_PML4: AtomicUsize = AtomicUsize::new(0);
static mut DISABLE_INVALIDATION: bool = true;

pub struct PageMapper {
    pml4_phys: u64, 
    is_current: bool,
    proc_id: usize,
    is_allocated: bool,
    page_reserve: [usize; 4],
    page_reserve_present: bool,
    is_kernel_pml4: bool
}

const RECURSIVE_SLOT: u64 = 511;
const TOTAL_ENTRIES: usize = 512;

impl PageMapper {
    // This is presently used only for initial address space creation
    // Therefore, it is written with assumption that virtual_address = physical_address
    #[cfg(not(test))]
    pub fn new(is_kernel_pml4: bool, proc_id: usize) -> Self {
        // All remaining address spaces are created by cloning the kernel virtual address space
        assert!(proc_id == 0 && is_kernel_pml4);
        let layout = Layout::from_size_align(PAGE_SIZE, PAGE_SIZE).unwrap();
        let pml4 = mem::allocate_memory(layout, 0)
                                .expect("Page base table allocation failed!") as usize;
        
        info!("Creating new address space with pml4 virtual address:{:#X}", pml4);

        // Initialize the page table (Recursive mapping)
        let raw_addr = pml4 as *mut u64;
        unsafe {
            raw_addr.write_bytes(0, TOTAL_ENTRIES);
            *raw_addr.add(RECURSIVE_SLOT as usize) = (pml4 as u64 & PTE::PHY_ADDR_MASK) | PTE::PWT | PTE::RW | PTE::P; 
        }

        // Create the PDPT for the kernel half of virtual address space
        for entry in TOTAL_ENTRIES / 2 .. TOTAL_ENTRIES - 1 {
            let pdpt = mem::allocate_memory(layout, 0)
                                .expect("Page directory pointer table allocation failed!") as *mut u64;
            
            unsafe {
                pdpt.write_bytes(0, TOTAL_ENTRIES);
                *raw_addr.add(entry) = (pdpt as u64 & PTE::PHY_ADDR_MASK) | PTE::PWT | PTE::RW | PTE::P; 
            }
        }
        
        KERNEL_PML4.store(pml4, Ordering::SeqCst);

        Self {
            pml4_phys: pml4 as u64,
            is_current: false,
            proc_id,
            is_allocated: false,
            page_reserve: [0; 4],
            page_reserve_present: false,
            is_kernel_pml4
        }
    }

    #[cfg(test)]
    pub fn new(_: bool, _: usize) -> Self {
        Self {
            pml4_phys: 0,
            is_current: false,
            proc_id: 0,
            is_allocated: false,
            page_reserve: [0; 4],
            page_reserve_present: false,
            is_kernel_pml4: false
        }
    }

    // Clone the kernel address space from the current active address space
    pub fn clone(proc_id: usize, pml4_virt: usize) -> Self {
        assert!(proc_id != 0);
        let parent_pml4 = Self::recursive_map_addr(RECURSIVE_SLOT, RECURSIVE_SLOT, RECURSIVE_SLOT);
        
        let layout = Layout::from_size_align(PAGE_SIZE, PAGE_SIZE).unwrap();
        let pml4_phys = mem::allocate_memory(layout, 0)
                                .expect("Page base table allocation failed!");
        
        mem::map_page_table(pml4_virt, pml4_phys.addr(), proc_id)
        .expect("Failed to map pml4 to process address space");
        
        // Initialize the page table 
        unsafe {
            (pml4_virt as *mut u64).write_bytes(0, TOTAL_ENTRIES);
        }

        let raw_addr_half = unsafe {
            (pml4_virt as *mut u64).add(TOTAL_ENTRIES / 2)
        };

        let parent_addr_half = unsafe {
            (parent_pml4 as *mut u64).add(TOTAL_ENTRIES / 2)
        };


        // Share the page tables used by kernel virtual address space for the kernel half
        unsafe {
            copy_nonoverlapping(parent_addr_half, raw_addr_half, TOTAL_ENTRIES / 2 - 1);

            // Create the recursive page table mapping
            *(pml4_virt as *mut u64).add(RECURSIVE_SLOT as usize) = (pml4_phys as u64 & PTE::PHY_ADDR_MASK)
            | PTE::PWT | PTE::RW | PTE::P; 
        }


        mem::unmap_page_table(pml4_virt, proc_id)
        .expect("Unable to unmap pml4 page table during clone!");

        Self {
            pml4_phys: pml4_phys as u64,
            is_current: false,
            proc_id,
            is_allocated: false,
            page_reserve: [0; 4],
            page_reserve_present: false,
            is_kernel_pml4: false
        }
    }

    pub fn set_allocated(&mut self) {
        self.is_allocated = true;
    }

    fn set_current(&mut self) {
        let pml4 = asm::read_cr3() & PTE::PHY_ADDR_MASK;

        self.is_current = pml4 == self.pml4_phys;
    }

    pub fn set_address_space(&mut self) {
        self.is_current = true;
        unsafe {
            // Set page table as write through
            asm::write_cr3((self.pml4_phys & PTE::PHY_ADDR_MASK) | PTE::PWT);
        }
    }

    // Map the memory for process A but under process B context.
    // The current use case for this is when process B clones to create process A
    pub fn map_memory_non_self(&mut self,
        page_reserve: &[usize; 4],
        virt_addr: usize,
        phys_addr: usize, 
        size: usize, 
        flags: u8) {
        self.set_current();

        assert!(!self.is_current); 
        
        self.page_reserve = *page_reserve;
        self.page_reserve_present = true;
        self.map_memory(virt_addr, phys_addr, size, flags);
        self.page_reserve_present = false;
    }

    pub fn map_memory(&mut self, virt_addr: usize, phys_addr: usize, size: usize, flags: u8) {
        assert!(virt_addr & 0xfff == 0  && phys_addr & 0xfff == 0);

        let size = size + (size as *const u8).align_offset(PAGE_SIZE);
        let num_pages = ceil_div(size, PAGE_SIZE);

        assert!(num_pages > 0);

        self.set_current();

        let is_user = flags & mem::PageDescriptor::USER != 0;
        let mut is_mmio = flags & mem::PageDescriptor::MMIO != 0;
        let mut is_wc   = flags & mem::PageDescriptor::WC   != 0;
        let is_global_feature = CPU_FEATURES.get().unwrap().lock().pge;
        let is_pat_feature = CPU_FEATURES.get().unwrap().lock().pat;

        assert!(!(is_mmio && is_wc), "PageDescriptor::MMIO and WC are mutually exclusive");
        
        // If we don't have WC capability, fallback to MMIO
        if is_wc && !is_pat_feature {
            is_mmio = true;
            is_wc = false;
        }

        let pml4 = self.get_table_mut(
            PageLevel::PML4,
            0,
            0,
            0,
            self.pml4_phys as usize
        );
        
        let (mut old_pml4_idx, mut old_pdpt_idx, mut old_pd_idx, _) =
            Self::split_indices(virt_addr as u64);

        let mut pdpt: *mut [u64; TOTAL_ENTRIES] =
            self.get_or_alloc_table(
                pml4,
                old_pml4_idx,
                PageLevel::PDPT,
                old_pml4_idx,
                0,
                0
            );

        let mut pd: *mut [u64; TOTAL_ENTRIES] =
            self.get_or_alloc_table(
                pdpt,
                old_pdpt_idx,
                PageLevel::PD,
                old_pml4_idx,
                old_pdpt_idx,
                0
            );

        let mut pt: *mut [u64; TOTAL_ENTRIES] =
            self.get_or_alloc_table(
                pd,
                old_pd_idx,
                PageLevel::PT,
                old_pml4_idx,
                old_pdpt_idx,
                old_pd_idx
            );

        for i in 0..num_pages {
            let va = (virt_addr + i * PAGE_SIZE) as u64;
            let pa = (phys_addr + i * PAGE_SIZE) as u64;
            
            let (pml4_idx, pdpt_idx, pd_idx, pt_idx) =
                Self::split_indices(va);

            if old_pml4_idx != pml4_idx {
                if !self.is_current {
                    mem::unmap_page_table(pdpt.addr(), self.proc_id)
                    .expect("Failed to unmap pdpt from process address space");
                }

                pdpt = self.get_or_alloc_table(
                    pml4,
                    pml4_idx,
                    PageLevel::PDPT,
                    pml4_idx,
                    0,
                    0
                );
            }

            if old_pdpt_idx != pdpt_idx {
                if !self.is_current {
                    mem::unmap_page_table(pd.addr(), self.proc_id)
                    .expect("Failed to unmap pd from process address space");
                }

                pd = self.get_or_alloc_table(
                    pdpt,
                    pdpt_idx,
                    PageLevel::PD,
                    pml4_idx,
                    pdpt_idx,
                    0
                );
            }

            if old_pd_idx != pd_idx {
                if !self.is_current {
                    mem::unmap_page_table(pt.addr(), self.proc_id)
                    .expect("Failed to unmap pt from process address space");
                }

                pt = self.get_or_alloc_table(
                    pd,
                    pd_idx,
                    PageLevel::PT,
                    pml4_idx,
                    pdpt_idx,
                    pd_idx
                );
            }
            
            unsafe {
                core::ptr::write_volatile(
                    (*pt).as_mut_ptr().add(pt_idx),
                    (pa & PTE::PHY_ADDR_MASK)
                        | en_flag!(is_user, PTE::U)
                        | en_flag!(is_mmio || is_wc, PTE::PCD)
                        | en_flag!(is_mmio, PTE::PWT)
                        | en_flag!(
                            is_wc && 
                            is_pat_feature,
                            PTE::PAT
                        )
                        | en_flag!(
                            !is_user &&
                            is_global_feature,
                            PTE::G
                        )
                        | PTE::RW
                        | PTE::P
                );
            }

            self.invalidate_tlb(va);
        
            old_pml4_idx = pml4_idx;
            old_pdpt_idx = pdpt_idx;
            old_pd_idx = pd_idx;
        }

        self.unmap_page_tables(
            pml4.addr(),
            pdpt.addr(),
            pd.addr(),
            pt.addr()
        );
        
        core::sync::atomic::fence(Ordering::SeqCst);
    }

    pub fn unmap_memory(&mut self, virt_addr: usize, size: usize) {
        assert!(virt_addr & 0xfff == 0 && size & 0xfff == 0);

        let num_pages = ceil_div(size, PAGE_SIZE);

        assert!(num_pages > 0);

        self.set_current();

        assert!(self.is_current);

        for i in 0..num_pages {
            let va = (virt_addr + i * PAGE_SIZE) as u64;

            let (pml4_idx, pdpt_idx, pd_idx, pt_idx) =
                Self::split_indices(va);
            
            // This is low level API. At this point, it is callers responsibility
            // to ensure that this is a valid mapping. We don't do checks here
            let pt = self.get_table_mut(
                PageLevel::PT,
                pml4_idx,
                pdpt_idx,
                pd_idx,
                0
            );

            let entry = unsafe {
                core::ptr::read_volatile(
                    (*pt).as_ptr().add(pt_idx)
                )
            };

            // Unmap this entry
            assert!(entry & PTE::P != 0);

            unsafe {
                core::ptr::write_volatile(
                    (*pt).as_mut_ptr().add(pt_idx),
                    0
                );
            }

            self.invalidate_tlb(va);
        }
        
        core::sync::atomic::fence(Ordering::SeqCst);
    }

    // Provide a fixed virtual address for each level of the page table
    fn get_page_level_virtual_address(&self, level: PageLevel, phy_addr: usize) -> usize {
        assert!(!self.is_current && self.page_reserve_present);

        // We are creating the kernel address space itself
        // Ensure identity mapping 
        if self.is_kernel_pml4 {
            phy_addr
        }
        else {
            match level {
                PageLevel::PML4 => {
                    self.page_reserve[0]
                },
                PageLevel::PDPT => {
                    self.page_reserve[1]
                },
                PageLevel::PD => {
                    self.page_reserve[2]
                },
                PageLevel::PT => {
                    self.page_reserve[3]
                }
            }
        }
    }

    // This is not a perfect solution. There is a brief period (The time from which these IPIs are sent
    // and the time they are received by the other cores) where the other cores won't observe the new mapping.
    // However this only becomes a problem if 2 or more cores are 
    // a) sharing the same address space
    // b) actively working with a region of virtual memory whose mapping has been currently changed on one of the cores
    // Unless we're changing the physical mapping of one of the core regions that are mapped during page map init,
    // we won't run into situation b
    // However, the current design is to first unmap the region, and then remap it if needed
    // There is no provision to just change the mapping directly. This means that the 2 threads which are accessing this region
    // will have to undergo some sort of synchronization anyway to not enter race condition. 
    // Therefore, this problem can be avoided at a higher level than over here.
    pub fn invalidate_other_cores(desc: MemoryRegion) {
        let cur_core = super::get_core();
        let total_cores = cpu::get_total_cores();
        
        if unlikely(unsafe{DISABLE_INVALIDATION}) {
            return;
        }

        for core in 0..total_cores {
            if core != cur_core {
                notify_core(IPIRequestType::TlbInvalidate(desc.clone()), core);
            }
        }
    }

    fn invalidate_tlb(&self, virt_addr: u64) {
        if self.is_current {
            unsafe { asm::invlpg(VirtAddr::new(virt_addr as usize).get() as u64); }
        }
    }

    fn unmap_page_tables(&mut self, pml4: usize, pdpt: usize, pd: usize, pt: usize) {
        if self.is_current {
            return;
        }

        mem::unmap_page_table(pml4, self.proc_id).expect("Failed to unmap pml4 from process address space");        
        mem::unmap_page_table(pdpt, self.proc_id).expect("Failed to unmap pdpt from process address space");        
        mem::unmap_page_table(pd, self.proc_id).expect("Failed to unmap pd from process address space");        
        mem::unmap_page_table(pt, self.proc_id).expect("Failed to unmap pt from process address space");        
    }

    // Get a mutable reference to a page table at a given level and index using recursive mapping
    // If this address space is not active, then caller is expected to fetch the virtual address to which this page table is mapped
    // level -> Indicates which level page table user wants to access
    fn get_table_mut(&self, level: PageLevel, pml_idx: usize, pdpt_idx: usize, pd_idx: usize, phy_addr: usize) -> *mut [u64; 512] {
        let virt = if self.is_current {
            match level {
                PageLevel::PML4 => Self::recursive_map_addr(RECURSIVE_SLOT, RECURSIVE_SLOT, RECURSIVE_SLOT),
                PageLevel::PDPT => Self::recursive_map_addr(RECURSIVE_SLOT, RECURSIVE_SLOT, pml_idx as u64),
                PageLevel::PD => Self::recursive_map_addr(RECURSIVE_SLOT, pml_idx as u64, pdpt_idx as u64),
                PageLevel::PT => Self::recursive_map_addr(pml_idx as u64, pdpt_idx as u64, pd_idx as u64)
            }
        }
        else {
            let virt_addr = self.get_page_level_virtual_address(level, phy_addr);
            mem::map_page_table(virt_addr, phy_addr, self.proc_id).expect("Page table allocation failed!");

            virt_addr as u64
        };
        
        virt as *mut [u64; 512]
    }

    // Get or allocate the next-level table, and ensure it is mapped in the recursive region
    // table -> Parent table from which we're going to obtain the next level page table
    // idx -> index in parent table to which the next level page table points
    // level -> Should be the next level page table we want
    // Ex: if table is PDPT, then level must be PD and idx must be the PDPT entry that points to that PD
    fn get_or_alloc_table(&self, table: *mut [u64; 512], idx: usize, level: PageLevel, pml_idx: usize, pdpt_idx: usize, pd_idx: usize) -> *mut [u64; 512] {
        // Get the virtual address of the table we're interested in
        // If page table is not present, then allocate it first
        let entry = unsafe {
            core::ptr::read_volatile((*table).as_ptr().add(idx))
        };

        let addr = if entry & 1 == 0 {
            let addr = self.allocate_page_table(level);
            
            // Map the physical address to the upper level table
            unsafe {
                core::ptr::write_volatile(
                    (*table).as_mut_ptr().add(idx),
                    addr.1 as u64 & PTE::PHY_ADDR_MASK
                        | PTE::U
                        | PTE::PWT
                        | PTE::P
                        | PTE::RW
                );
            }

            Some(addr)
        }
        else {
            None
        };
        let vaddr = if self.is_current {
            // This address is valid if this address space were active
            let rec_addr = match level {
                PageLevel::PDPT => Self::recursive_map_addr(RECURSIVE_SLOT, RECURSIVE_SLOT, pml_idx as u64),
                PageLevel::PD => Self::recursive_map_addr(RECURSIVE_SLOT, pml_idx as u64, pdpt_idx as u64),
                PageLevel::PT => Self::recursive_map_addr(pml_idx as u64, pdpt_idx as u64, pd_idx as u64),
                _ => {
                    panic!("get_or_alloc_table() called with level: PML4");
                }
            } as usize;
            
            // If we had just mapped that memory, need to invalidate this region to make it visible
            if addr.is_some() {
                unsafe {
                    asm::invlpg(VirtAddr::new(rec_addr).get() as u64);
                }
            }

            rec_addr
        }
        else {
            if let Some(val) = &addr {
                val.0
            }
            else {
                // Page table was already exists in physical memory.
                // Map it to current process's address space
                let phys = (entry & PTE::PHY_ADDR_MASK) as usize;
                let virt_addr = self.get_page_level_virtual_address(level, phys);
                mem::map_page_table(virt_addr, phys, self.proc_id)
                .expect("Page table could not be mapped to current process space");

                virt_addr
            }
        };

        // If table was allocated now, then initialize it
        if addr.is_some() {
            unsafe {
                (vaddr as *mut u64).write_bytes(0, TOTAL_ENTRIES);
            }
        }
        
        // Using the table's virtual address, get reference to actual table
        // This virtual address may be in recursive region or in some region in caller's memory
        vaddr as *mut [u64; 512]
    }

    //pub fn get_table_mapping(&mut self, virt_addr: u64) -> usize {
    //    self.set_current();

    //    assert!(self.is_current);

    //    let (pml4_idx, pdpt_idx, pd_idx, pt_idx) = Self::split_indices(virt_addr);
    //    let pt = self.get_table_mut(PageLevel::PT, pml4_idx, pdpt_idx, pd_idx, 0);

    //    let phy_addr = pt[pt_idx] as usize;

    //    phy_addr
    //}

    // Allocates 1 page table and returns it's virtual and physical memory
    fn allocate_page_table(&self, level: PageLevel) -> (usize, usize) {
        // If current active space, then just give the physical memory as caller will recursively map it
        let layout = Layout::from_size_align(PAGE_SIZE, PAGE_SIZE).unwrap();
        if self.is_current {
            let phy_addr = mem::allocate_memory(layout, 0).expect("Page table allocation failed!") as usize;
            (phy_addr, phy_addr)
        }
        else {
            let phy_addr = mem::allocate_memory(layout, 0).expect("Page table allocation failed!") as usize;
            let virt_addr = self.get_page_level_virtual_address(level, phy_addr);

            mem::map_page_table(virt_addr, phy_addr, self.proc_id)
            .expect("Page table could not be mapped to current process space");
            (virt_addr, phy_addr)
        }
    }

    // Compute the recursive mapping address for a page table at a given level and indices
    fn recursive_map_addr(pml: u64, pdpt: u64, pd: u64) -> u64 {
        // Since memory address needs to be canonical, we use 0xffffff instead of 0x1ff
        (0x1ffffff << 39) |
        ((pml & 0x1ff) << 30) |
        ((pdpt & 0x1ff) << 21) |
        ((pd & 0x1ff) << 12)
    }
    
    fn split_indices(virt_addr: u64) -> (usize, usize, usize, usize) {
        let pml4 = (virt_addr >> 39) & 0x1ff;
        let pdpt = (virt_addr >> 30) & 0x1ff;
        let pd = (virt_addr >> 21) & 0x1ff;
        let pt = (virt_addr >> 12) & 0x1ff;
        (pml4 as usize, pdpt as usize, pd as usize, pt as usize)
    }

    // We must have been switched out from this address space
    pub fn destroy_page_tables(&mut self, page_reserve: &[usize; 4]) {
        self.set_current();

        assert!(self.proc_id != 0, "Attempted to destroy kernel address space!");
        assert!(!self.is_current, "Address space being destroyed while currently selected!");
        assert!(self.is_allocated, "destroy_page_table called on destroyed address space!");

        self.page_reserve = *page_reserve;
        self.page_reserve_present = true;

        // 0 is fine here since it is guaranteed that we are way past initial address space creation
        // at this point
        let pml4_virt = self.get_page_level_virtual_address(PageLevel::PML4, 0);
        let pdpt_virt = self.get_page_level_virtual_address(PageLevel::PDPT, 0);
        let pd_virt = self.get_page_level_virtual_address(PageLevel::PD, 0);
        
        mem::map_page_table(pml4_virt, self.pml4_phys as usize, self.proc_id)
        .expect("Unable to map pml4 to process address space");

        let pml4 = usize_to_ptr::<[u64; TOTAL_ENTRIES]>(pml4_virt);
        
        let mut page_tables = 0;
        crate::mem_log!("Destroying page tables for process {}", self.proc_id);
        
        // Go over PML4 entries 
        let layout = Layout::from_size_align(PAGE_SIZE, PAGE_SIZE).unwrap();

        // Only remove the page tables associated with user half of the memory
        for pml4_entry in 0..TOTAL_ENTRIES / 2 {
            let pml4e = unsafe {
                core::ptr::read_volatile(
                    (*pml4).as_ptr().add(pml4_entry)
                )
            };

            if (pml4e & PTE::P) == 0 {
                continue;
            }

            let pdpt_phy = pml4e & PTE::PHY_ADDR_MASK;

            mem::map_page_table(pdpt_virt, pdpt_phy as usize, self.proc_id)
            .expect("Unable to map pdpt to process address space");

            let pdpt = usize_to_ptr::<[u64; TOTAL_ENTRIES]>(pdpt_virt);

            for pdpt_entry in 0..TOTAL_ENTRIES {
                let pdpte = unsafe {
                    core::ptr::read_volatile(
                        (*pdpt).as_ptr().add(pdpt_entry)
                    )
                };

                if (pdpte & PTE::P) == 0 {
                    continue;
                }

                let pd_phy = pdpte & PTE::PHY_ADDR_MASK;

                mem::map_page_table(pd_virt, pd_phy as usize, self.proc_id)
                .expect("Unable to map pd to process address space");

                let pd = usize_to_ptr::<[u64; TOTAL_ENTRIES]>(pd_virt);

                for pd_entry in 0..TOTAL_ENTRIES {
                    let pde = unsafe {
                        core::ptr::read_volatile(
                            (*pd).as_ptr().add(pd_entry)
                        )
                    };

                    if (pde & PTE::P) == 0 {
                        continue;
                    }

                    let pt_phy = pde & PTE::PHY_ADDR_MASK;

                    // Deallocate PT
                    mem::deallocate_memory(pt_phy as *mut u8, layout, 0)
                    .expect("Failed to deallocate PT");

                    page_tables += 1;
                }

                // All entries within this PD have been deallocated. Now, deallocate this PD
                mem::unmap_page_table(pd.addr(), self.proc_id)
                .expect("Failed to unmap PD from process address space");
                
                // Deallocate PD
                mem::deallocate_memory(pd_phy as *mut u8, layout, 0)
                .expect("Failed to deallocate PD");

                page_tables += 1;
            }
            
            // All entries within this PDPT have been deallocated. Now, deallocate this PDPT
            mem::unmap_page_table(pdpt.addr(), self.proc_id)
            .expect("Failed to unmap PDPT from process address space");
            
            // Deallocate PDPT
            mem::deallocate_memory(pdpt_phy as *mut u8, layout, 0)
            .expect("Failed to deallocate PDPT");

            page_tables += 1;
        }
        
        // All entries within PML4 have been deallocated.
        mem::unmap_page_table(pml4.addr(), self.proc_id)
        .expect("Failed to unmap PML4 from process address space");
        
        // Deallocate PML4
        mem::deallocate_memory(self.pml4_phys as *mut u8, layout, 0)
        .expect("Failed to deallocate PML4");

        page_tables += 1;

        self.is_allocated = false;
        self.page_reserve_present = false;

        crate::mem_log!("Destroyed {} page tables", page_tables);
    }
}

#[allow(dead_code)]
pub fn enable_invalidation() {
    unsafe {
        DISABLE_INVALIDATION = false;
    }
}

pub fn get_kernel_pml4() -> usize {
    KERNEL_PML4.load(Ordering::SeqCst)
}