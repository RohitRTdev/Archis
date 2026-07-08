use crate::REMAP_LIST;
use crate::cpu::{self, PerCpu};
use crate::hal::PAGE_FAULT_VECTOR;
use crate::mem::{KERNEL_HALF_OFFSET, KERNEL_HALF_OFFSET_RAW, PageDescriptor, fixed_allocator::Regions::*};
use crate::sync::{Once, Spinlock};
use crate::hal::{self, PageMapper, try_kill_user_process};
use crate::mem::FixedList;
use kernel_intf::list::{List, ListNode};
use crate::cpu::MAX_CPUS;
use kernel_intf::KError;
use kernel_intf::{info, debug};
use crate::RemapType::*;
use core::alloc::Layout;
use core::ptr::{null_mut, NonNull};
use core::hint::likely;
use core::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use common::{MemoryRegion, PAGE_SIZE, ceil_div, ptr_to_ref_mut};
use super::PHY_MEM_CB;

const ERROR_MESSAGE: &'static str = "System in bad state. Critical memory failure";

#[derive(PartialEq)]
pub enum MapFetchType {
    Any,
    Kernel
}

pub struct VirtMemConBlk {
    total_memory: usize,
    avl_memory: usize,
    free_block_list: FixedList<PageDescriptor, {Region0 as usize}>,
    alloc_block_list: FixedList<PageDescriptor, {Region0 as usize}>,
    page_mapper: PageMapper,
    proc_id: usize
}

static IS_ADDR_SPACE_INIT: AtomicBool = AtomicBool::new(false);
static ADDRESS_SPACES: Once<Spinlock<FixedList<Spinlock<VirtMemConBlk>, {Region1 as usize}>>> = Once::new();
static KERN_ADDR_SPACE: Once<AtomicPtr<Spinlock<VirtMemConBlk>>> = Once::new();

// We cannot use Arc or something similar since this is activated prior to heap initialization
static PER_CPU_ACTIVE_VCB: PerCpu<AtomicPtr<Spinlock<VirtMemConBlk>>> = PerCpu::new_with(
    [const {AtomicPtr::new(core::ptr::null_mut())}; MAX_CPUS]
);

pub type VCB = NonNull<Spinlock<VirtMemConBlk>>;

impl VirtMemConBlk {
    // Only used for process 0 address space creation
    #[cfg(all(target_arch="x86_64", not(test)))]
    fn new(is_init_address_space: bool) -> Self {
        // Since virtual address has max size of 48 bits
        // But from address 0x1ff << 39 onwards we reserve for page tables, so don't use it for conventional memory
        // We decrement one page, since we don't want page 0 in virtual address space

        let total_memory = (0x1ff << 39) - PAGE_SIZE;
        let num_pages_user = ceil_div(KERNEL_HALF_OFFSET_RAW - PAGE_SIZE, PAGE_SIZE);

        let num_pages_kernel = ceil_div((0x1ff << 39) - KERNEL_HALF_OFFSET_RAW, PAGE_SIZE);
        let mut free_block_list= List::new();
        
        // Create separate blocks for user and kernel memory
        free_block_list.add_node(PageDescriptor {
            num_pages: num_pages_user, start_phy_address: 0, start_virt_address: PAGE_SIZE, flags: 0, is_mapped: false
        }).unwrap();
        
        free_block_list.add_node(PageDescriptor {
            num_pages: num_pages_kernel, start_phy_address: 0, start_virt_address: KERNEL_HALF_OFFSET, flags: 0, is_mapped: false
        }).unwrap();

        Self {
            total_memory,
            avl_memory: total_memory,
            free_block_list,
            alloc_block_list: List::new(),
            page_mapper: PageMapper::new(is_init_address_space, 0),
            proc_id: 0 
        }
    }
    
    #[cfg(test)]
    fn new(_: bool) -> Self {
        let total_memory = (0x1ff << 39) - PAGE_SIZE;
        let num_pages_user = ceil_div(KERNEL_HALF_OFFSET_RAW - PAGE_SIZE, PAGE_SIZE);

        let num_pages_kernel = ceil_div((0x1ff << 39) - KERNEL_HALF_OFFSET_RAW, PAGE_SIZE);
        let mut free_block_list= List::new();
        
        // Create separate blocks for user and kernel memory
        free_block_list.add_node(PageDescriptor {
            num_pages: num_pages_user, start_phy_address: 0, start_virt_address: PAGE_SIZE, flags: 0, is_mapped: false
        }).unwrap();
        
        free_block_list.add_node(PageDescriptor {
            num_pages: num_pages_kernel, start_phy_address: 0, start_virt_address: KERNEL_HALF_OFFSET, flags: 0, is_mapped: false
        }).unwrap();

        // Allocate in process 1 to allow user blocks too
        Self {
            total_memory,
            avl_memory: total_memory,
            free_block_list,
            alloc_block_list: List::new(),
            page_mapper: PageMapper::new(true, 1),
            proc_id: 1 
        }
    }

    fn find_best_fit(&mut self, pages: usize, is_user: bool) -> Result<*mut u8, KError> {
        let mut smallest_blk: Option<&mut ListNode<PageDescriptor>> = None;

        // Track the block with the smallest number of pages that can satisfy above request
        // For kernel pages, make sure that allocated address is above KERNEL_HALF_OFFSET
        for block in self.free_block_list.iter_mut() {
            if block.num_pages >= pages && 
            ((is_user && block.start_virt_address < KERNEL_HALF_OFFSET) || (!is_user && block.start_virt_address >= KERNEL_HALF_OFFSET)) {
                if let Some(val) = &smallest_blk {
                    if block.num_pages < val.num_pages {
                        smallest_blk = Some(block);
                    }
                }
                else {
                    smallest_blk = Some(block);
                }
            }
        }

        if let Some(node) = smallest_blk {
            node.num_pages -= pages;
            let start_address = node.start_virt_address as *mut u8;

            node.start_virt_address += pages * PAGE_SIZE;
            if node.num_pages == 0 {
                let list_node = NonNull::from(node);
                unsafe {
                    self.free_block_list.remove_node(list_node);
                }
            }

            return Ok(start_address);
        }
        else {
            return Err(KError::OutOfMemory);
        }
    }

    fn coalesce_block(&mut self, addr: usize, num_pages: usize) {
        let mut found_blk = None; 
        let num_size = num_pages * PAGE_SIZE;

        // Check if this block can be merged with an existing block
        for blk in self.free_block_list.iter_mut() {
            // Keep kernel and user blocks separate
            if blk.start_virt_address + blk.num_pages * PAGE_SIZE == addr && addr != KERNEL_HALF_OFFSET {
                blk.num_pages += num_pages;
                found_blk = Some(NonNull::from(&*blk));
                break;
            }
            else if addr + num_size == blk.start_virt_address && blk.start_virt_address != KERNEL_HALF_OFFSET {
                blk.start_virt_address -= num_size;
                blk.num_pages += num_pages;
                found_blk = Some(NonNull::from(&*blk));
                break;
            }
        }

        // Now run same algorithm once more (There could be atmost 2 blocks to which a fragmented block could be merged)        
        if let Some(blk) = found_blk {
            let blk_desc = unsafe {blk.as_ref()};
            let merge_blk: Option<&mut ListNode<PageDescriptor>> = self.free_block_list.iter_mut().find(|item| {
                (item.start_virt_address + item.num_pages * PAGE_SIZE == blk_desc.start_virt_address && blk_desc.start_virt_address != KERNEL_HALF_OFFSET) || 
                (blk_desc.start_virt_address + blk_desc.num_pages * PAGE_SIZE == item.start_virt_address && item.start_virt_address != KERNEL_HALF_OFFSET) 
            });

            // We found one more block to which the new block can be merged
            // In this case all three blocks are merged as one
            if let Some(merge_blk_desc) = merge_blk {
                merge_blk_desc.num_pages += blk_desc.num_pages;
                merge_blk_desc.start_virt_address = blk_desc.start_virt_address.min(merge_blk_desc.start_virt_address);
                unsafe {
                    self.free_block_list.remove_node(blk);
                }
            }
        } 
        else {
            // If no block to which the fragmented region can be merged, just create a new block to describe the free region
            // If it fails at this point, it's hard to recover
            self.free_block_list.add_node(PageDescriptor { num_pages, start_phy_address: 0, start_virt_address: addr as usize, flags: 0, is_mapped: false })
            .expect(ERROR_MESSAGE);
        }
    }

    // Unlike allocate, reserve virtual space can reserve virtual memory anywhere
    // In normal allocate, if caller requests kernel memory, then it is allocated from range
    // KERNEL_HALF_OFFSET..MAX_VIRTUAL_ADDRESS (minus the page table recursive addr range)
    // Reserve virtual space breaks this structure. This is primarily used to allow caller to reserve identity mapped
    // regions as kernel memory. As these regions are mostly below the KERNEL_HALF_OFFSET region, this API is required
    fn reserve_virtual_space(&mut self, virt_addr: usize, layout: Layout) -> Result<(), KError> {
        let size = layout.size() + (layout.size() as *const u8).align_offset(PAGE_SIZE);
        
        // Find a superset of the region that the user is interested in
        let blk = self.free_block_list.iter().find(|item| {
            virt_addr >= item.start_virt_address 
            && virt_addr + size <= item.start_virt_address + item.num_pages * PAGE_SIZE
        });
        
        if let Some(desc) = blk {
            let top = PageDescriptor {
                num_pages: ceil_div(virt_addr - desc.start_virt_address, PAGE_SIZE),
                start_phy_address: 0,
                start_virt_address: desc.start_virt_address,
                flags: 0,
                is_mapped: false
            };

            let middle = PageDescriptor {
                num_pages: ceil_div(size, PAGE_SIZE),
                start_phy_address: 0,
                start_virt_address: virt_addr,
                flags: 0,
                is_mapped: false
            };

            let bottom = PageDescriptor {
                num_pages: ceil_div(desc.num_pages * PAGE_SIZE  - ((virt_addr + size) - desc.start_virt_address), PAGE_SIZE),
                start_phy_address: 0,
                start_virt_address: virt_addr + size,
                flags: 0,
                is_mapped: false
            };

            unsafe {
                self.free_block_list.remove_node(NonNull::from(desc));
            }

            for descriptor in [top, bottom] {
                if descriptor.num_pages != 0 {
                    self.free_block_list.add_node(descriptor).expect(ERROR_MESSAGE);
                }
            }

            self.alloc_block_list.add_node(middle).expect(ERROR_MESSAGE);
        }
        else {
            debug!("alloc_block_list={:?}, free_block_list={:?}", self.alloc_block_list, self.free_block_list);
            info!("reserve_virtual_space could not reserve memory of size:{} at address:{:#X}", size, virt_addr);
            return Err(KError::OutOfMemory);
        }
    
        Ok(())
    }

    // Finds a suitable block in virtual address space and reserves it
    fn allocate(&mut self, layout: Layout, is_user: bool) -> Result<*mut u8, KError> {
        if layout.size() >= self.avl_memory || layout.size() > self.total_memory {
            return Err(KError::OutOfMemory);
        }

        if layout.align() > PAGE_SIZE {
            return Err(KError::InvalidArgument);
        }

        assert!(!(self.proc_id == 0 && is_user), "Kernel virtual address space must not allocate user blocks!");

        let num_pages = ceil_div(layout.size(), PAGE_SIZE);
        let virt_addr = self.find_best_fit(num_pages, is_user)?;    
        self.avl_memory -= num_pages * PAGE_SIZE;
        let flags = if is_user { PageDescriptor::USER } else { 0 };
        // Now we have got virtual address
        self.alloc_block_list.add_node(PageDescriptor { num_pages, start_phy_address: 0, 
            start_virt_address: virt_addr as usize, flags, is_mapped: false}).expect(ERROR_MESSAGE);

        Ok(virt_addr)
    }

    // Removes the allocation from the virtual address space
    // Page must be unmapped first
    fn deallocate(&mut self, addr: *mut u8, layout: Layout) -> Result<(), KError> {
        if layout.align() > PAGE_SIZE {
            return Err(KError::InvalidArgument);
        }

        let num_pages = ceil_div(layout.size(), PAGE_SIZE);
        let num_size = num_pages * PAGE_SIZE;

        // Remove node from alloc_block_list
        let mut alloc_blk = None;
        for blk in self.alloc_block_list.iter() {
            if blk.start_virt_address == addr as usize && blk.num_pages == num_pages {
                alloc_blk = Some(NonNull::from(blk));
                
                // It is required for the memory being deallocated to not have been mapped to physical memory
                if blk.is_mapped {
                    return Err(KError::InvalidArgument);
                }
                break;
            }
        }

        if let Some(blk) = alloc_blk {
            unsafe {
                self.alloc_block_list.remove_node(blk);
            }
        }
        else {
            // In case caller tries to free memory which has not been allocated, then we return here
            debug!("{:?}", self.alloc_block_list);
            return Err(KError::InvalidArgument);
        } 

        self.coalesce_block(addr as usize, num_pages);
        self.avl_memory += num_size;

        Ok(())
    }

    fn get_phys_address(&mut self, virt_addr: usize) -> Option<usize> {
        // Check all locations linearly to get the physical address
        for blk in self.alloc_block_list.iter() {
            if blk.start_virt_address <= virt_addr && blk.start_virt_address + blk.num_pages * PAGE_SIZE > virt_addr
            && blk.is_mapped {
                return Some(blk.start_phy_address + virt_addr - blk.start_virt_address);
            }
        }

        None
    }

    fn get_virt_address(&mut self, phys_addr: usize, fetch_type: MapFetchType) -> Option<usize> {
        // Check all locations linearly to get the virtual address
        let mut lowest_address = None;
        for blk in self.alloc_block_list.iter() {
            if blk.start_phy_address <= phys_addr && blk.start_phy_address + blk.num_pages * PAGE_SIZE > phys_addr 
            && blk.is_mapped {
                let new_addr = blk.start_virt_address + phys_addr - blk.start_phy_address;  
                if lowest_address.is_none() {
                    lowest_address = Some(new_addr);
                }
                else {
                    lowest_address = lowest_address.and_then(|addr: usize| {
                        // In this case unconditionally set the preferred address as the kernel address even though previous address is lower
                        if fetch_type == MapFetchType::Kernel && new_addr >= KERNEL_HALF_OFFSET && addr < KERNEL_HALF_OFFSET {
                            Some(new_addr)
                        }
                        else {
                            Some(addr.min(new_addr))
                        }
                    });
                }
            }
        }

        #[cfg(debug_assertions)]
        if lowest_address.is_none() {
            debug!("phys_addr={}, alloc_block_list={:?}", phys_addr, self.alloc_block_list);
        }

        lowest_address
    }

    // Allowed to map memory only if the address is reserved in the virtual address space
    // In case user wants to map new physical address to existing virtual address, then first unmap the memory
    // and then map the new physical address 
    fn map_memory(&mut self, phys_addr: usize, virt_addr: usize, size: usize, flags: u8, skip_map: bool) -> Result<(), KError> {
        let size = size + (size as *const u8).align_offset(PAGE_SIZE);
        let is_user = (flags & PageDescriptor::USER) != 0;
        if phys_addr & (PAGE_SIZE - 1) != 0 || virt_addr & (PAGE_SIZE - 1) != 0 {
            return Err(KError::InvalidArgument);
        }
        
        assert!(!(self.proc_id == 0 && is_user), "Kernel virtual address space must not map user blocks!");

        // Only kernel virtual address space handles any allocations made to kernel memory
        // Other address spaces simply need to allocate it within their page tables
        if self.proc_id != 0 && !is_user {
            self.page_mapper.map_memory(virt_addr, phys_addr, size, flags);
            return Ok(())
        }

        // Try to find a block that is reserved in the virtual address space and that is unmapped
        // The range we're trying to find is a superset of the range that the caller is interested in (Previously reserved using allocate/reserve_virtual_space)
        let blk = self.alloc_block_list.iter().find(|item| {
            virt_addr >= item.start_virt_address
            && virt_addr + size <= item.start_virt_address + item.num_pages * PAGE_SIZE 
            && !item.is_mapped
        });
        
        if let Some(desc) = blk {
            let top = PageDescriptor {
                num_pages: ceil_div(virt_addr - desc.start_virt_address, PAGE_SIZE),
                start_phy_address: 0,
                start_virt_address: desc.start_virt_address,
                flags: 0,
                is_mapped: false
            };

            let middle = PageDescriptor {
                num_pages: ceil_div(size, PAGE_SIZE),
                start_phy_address: phys_addr,
                start_virt_address: virt_addr,
                flags,
                is_mapped: true
            };

            let bottom = PageDescriptor {
                num_pages: ceil_div(desc.num_pages * PAGE_SIZE  - ((virt_addr + size) - desc.start_virt_address), PAGE_SIZE),
                start_phy_address: 0,
                start_virt_address: virt_addr + size,
                flags: 0,
                is_mapped: false
            };
            
            unsafe {
                self.alloc_block_list.remove_node(NonNull::from(desc));
            }
            
            for descriptor in [top, bottom] {
                if descriptor.num_pages != 0 {
                    self.alloc_block_list.add_node(descriptor).expect(ERROR_MESSAGE);
                }
            }

            self.alloc_block_list.add_node(middle).expect(ERROR_MESSAGE);
        }
        else {
            debug!("alloc_block_list={:?}, free_block_list={:?}", self.alloc_block_list, self.free_block_list);
            info!("map_memory could not reserve memory of size:{} at address:{:#X}", size, virt_addr);
            return Err(KError::InvalidArgument);
        }

        if !skip_map {
            self.page_mapper.map_memory(virt_addr, phys_addr, size, flags);
        }
        
        Ok(())
    }

    // This unmaps the memory, but the virtual address space would still be reserved
    fn unmap_memory(&mut self, virt_addr: usize, size: usize, is_user: bool, skip_map: bool) -> Result<*mut u8, KError> {
        let size = size + (size as *const u8).align_offset(PAGE_SIZE);  
        
        let num_pages = ceil_div(size, PAGE_SIZE);
        if virt_addr as usize & (PAGE_SIZE - 1) != 0 {
            return Err(KError::InvalidArgument);
        }

        if self.proc_id != 0 && !is_user {
            self.page_mapper.unmap_memory(virt_addr, size);
            return Ok(null_mut());
        }

        // There should be an exact block in the allocated list (Previously reserved by allocate/reserve_virtual_space and mapped by map_memory)
        let blk = self.alloc_block_list.iter_mut().find(|item| {
            item.start_virt_address == virt_addr && item.num_pages == num_pages && item.is_mapped
        });

        let phy_addr; 
        if let Some(desc) = blk {
            desc.is_mapped = false;

            phy_addr = desc.start_phy_address;
            desc.start_phy_address = 0;
        }
        else {
            // In case caller tries to free memory which has not been allocated, then we return here
            return Err(KError::InvalidArgument);
        } 

        if !skip_map {
            self.page_mapper.unmap_memory(virt_addr, size);
        }
    
        Ok(phy_addr as *mut u8)
    }

    fn get_page_reserve() -> Result<[usize; 4], KError> {
        let kernel_addr_space = get_kernel_addr_space();
        let mut page_reserve = [0; 4];
        for page in 0..4 {
            page_reserve[page] = unsafe {
                (*kernel_addr_space.as_ptr()).lock().allocate(
                    Layout::from_size_align(PAGE_SIZE, PAGE_SIZE).unwrap(),
                    false
                )?
            } as usize;
        }

        Ok(page_reserve)
    }

    fn remove_page_reserve(page_reserve: &[usize; 4]) {
        let kernel_addr_space = get_kernel_addr_space();
        for page in 0..4 {
            unsafe {
                (*kernel_addr_space.as_ptr()).lock().deallocate(
                    page_reserve[page] as *mut u8,
                    Layout::from_size_align(PAGE_SIZE, PAGE_SIZE).unwrap()
                ).expect("Failed to deallocate reserve page tables!");
            }
        }
    }

    pub fn clone(parent_vcb: VCB, proc_id: usize) -> Result<VCB, KError> {
        crate::mem_log!("Cloning kernel virtual address space for process id {}", proc_id);
        let (free_list, alloc_list, total_mem, avl_mem) = {
            let parent_vcb = unsafe {
                (*parent_vcb.as_ptr()).lock()
            };

            assert_eq!(parent_vcb.proc_id, 0, "Only kernel virtual address space can be cloned!");

            let mut free_list = List::new();
            let mut avl_mem = 0;

            // Any VCB except the kernel virtual address space must carry only user space allocatable blocks
            parent_vcb.free_block_list.iter().filter(|blk| {
                blk.start_virt_address < KERNEL_HALF_OFFSET
            }).for_each(|blk| {
                avl_mem += blk.num_pages * PAGE_SIZE;
                free_list.add_node((&**blk).clone()).expect("Unable to clone free list node to new address space");
            });

            let alloc_list = parent_vcb.alloc_block_list.clone();
            let total_mem = parent_vcb.total_memory;


            (free_list, alloc_list, total_mem, avl_mem)
        };

        let page_reserve = Self::get_page_reserve()?;
        let mut page_mapper = PageMapper::clone(proc_id, page_reserve[0]);

        for blk in alloc_list.iter() {
            // Only need to explicitly map those blocks that are part of kernel memory
            // but not in the kernel half
            if blk.is_mapped && blk.start_virt_address < KERNEL_HALF_OFFSET {
                // If page mapper fails, kernel panics, right now we don't have a fallback
                crate::mem_log!("Mapping memory blk => Virtual={:#X}, physical={:#X}, pages={:#X}, is_mmio={}",
            blk.start_virt_address, blk.start_phy_address, blk.num_pages, blk.flags & PageDescriptor::MMIO != 0);

                page_mapper.map_memory_non_self(
                    &page_reserve,
                    blk.start_virt_address,
                    blk.start_phy_address,
                    blk.num_pages * PAGE_SIZE,
                    blk.flags
                );
            }
        }

        Self::remove_page_reserve(&page_reserve);

        page_mapper.set_allocated();

        let new_virtual_allocator = Self {
            total_memory: total_mem,
            avl_memory: avl_mem,
            free_block_list: free_list,
            alloc_block_list: List::new(),
            page_mapper,
            proc_id
        };

        let mut addr_space_list = ADDRESS_SPACES.get().unwrap().lock();
        addr_space_list.add_node(Spinlock::new(new_virtual_allocator))?;

    
        let new_vcb = NonNull::from(&**addr_space_list.last().unwrap());
        Ok(new_vcb)
    }

    // This address space must not be a part of any core
    pub unsafe fn destroy_address_space(vcb: VCB) {
        #[cfg(debug_assertions)]
        {
            let total_cores = cpu::get_total_cores();
            for core in 0..total_cores {
                let active_vcb = get_active_vcb_for_core(core);
                debug_assert!(active_vcb != vcb);
            }
        }

        {
            let vcb = {
                // Remove this vcb from the list of address spaces
                let mut addr_spaces = ADDRESS_SPACES.get().unwrap().lock();

                addr_spaces.find_and_remove(|this_vcb| {
                    NonNull::from(this_vcb) == vcb
                }).expect("Critical error: Address space could not be found in ADDRESS_SPACES list!")
            };

            let page_reserve = Self::get_page_reserve()
            .expect("System in bad state. Could not reserve page table for destroying process page tables!");

            // Release the address space lock before continuing    
            vcb.lock().page_mapper.destroy_page_tables(&page_reserve);

            Self::remove_page_reserve(&page_reserve);
        }
    }
}

// This is only to be called from scheduler (when scheduler lock is held)
pub unsafe fn set_address_space(vcb: VCB) {
    PER_CPU_ACTIVE_VCB.local().store(vcb.as_ptr() as *mut _, Ordering::Release);

    unsafe { vcb.as_ref().lock().page_mapper.set_address_space(); }
}

pub fn get_kernel_addr_space() -> VCB {
    NonNull::new(KERN_ADDR_SPACE.get().unwrap().load(Ordering::Relaxed)).unwrap()
}

fn get_active_vcb() -> NonNull<Spinlock<VirtMemConBlk>> {
    let vcb = PER_CPU_ACTIVE_VCB.local().load(Ordering::Acquire);

    NonNull::new(vcb).unwrap()
}

fn get_active_vcb_for_core(core: usize) -> NonNull<Spinlock<VirtMemConBlk>> {
    let total_cores = cpu::get_total_cores();
    assert!(core < total_cores);
    
    let vcb = unsafe {
        PER_CPU_ACTIVE_VCB.get(core)
    }.load(Ordering::Acquire);

    NonNull::new(vcb).unwrap()
}

#[allow(dead_code)]
pub fn ap_init() {
    let kernel_addr_space = unsafe {
        PER_CPU_ACTIVE_VCB.get(0).load(Ordering::Acquire)
    };

    // All AP will use the same kernel virtual address space
    PER_CPU_ACTIVE_VCB.local().store(kernel_addr_space, Ordering::Release);
}


// When the page mapper needs to map page to current address space, but the page mapper
// is not the active mapper (This happens when mapping kernel memory during process clone & destruction phase),
// so it uses this utility function. This temporarily maps the page table onto the 
// current active address space so that page mapper can access it
pub fn map_page_table(virt_addr: usize, phy_addr: usize, proc_id: usize) -> Result<(), KError> {
    if likely(IS_ADDR_SPACE_INIT.load(Ordering::Relaxed)) {
        let active_vcb = get_active_vcb();

        unsafe {
            // Kernel address space is not the active address space, but it's requesting page table
            // In this case, retrieve pre-allocated virtual address
            // This is safe to do here, since kernel address space lock is held and no other task can request from the reserved space 
            assert!(proc_id != 0);

            // In case this is not kernel address space, then this won't update the 
            // control structures. However, it's fine since no other thread would deliberately 
            // try to map a random virtual address range since its reserved by this address space
            // before the cloning started
            (*active_vcb.as_ptr()).lock().map_memory(phy_addr, virt_addr, PAGE_SIZE, 0, false)?;
        }
    }
    else {
        // Do nothing
    }

    Ok(())
}

pub fn map_to_kernel(phys_addr: usize, size: usize) -> Result<*mut u8, KError> {
    let layout = Layout::from_size_align(size, PAGE_SIZE).unwrap();
    let virt_addr = allocate_memory(layout, PageDescriptor::VIRTUAL | PageDescriptor::NO_ALLOC)?;
    map_memory(phys_addr, virt_addr.addr(), size, PageDescriptor::VIRTUAL)?;

    Ok(virt_addr)
} 

pub fn unmap_from_kernel(virt_addr: usize, size: usize) -> Result<(), KError> {
    let layout = Layout::from_size_align(size, PAGE_SIZE).unwrap();

    unmap_memory(virt_addr, size, PageDescriptor::VIRTUAL)?;
    deallocate_memory(virt_addr as *mut u8, layout, PageDescriptor::VIRTUAL | PageDescriptor::NO_ALLOC)
} 

// When the page mapper needs to unmap page from current address space, but the page mapper
// is not the active mapper, so it uses this utility function
pub fn unmap_page_table(virt_addr: usize, proc_id: usize) -> Result<(), KError> {
    if likely(IS_ADDR_SPACE_INIT.load(Ordering::Relaxed)) {
        let active_vcb = get_active_vcb();

        unsafe {
            assert!(proc_id != 0);
            (*active_vcb.as_ptr()).lock().unmap_memory(virt_addr,  PAGE_SIZE, false, false)?;
        }
    }
    else {
        // Do nothing
    }

    Ok(())
}

#[allow(dead_code)]
pub fn reserve_virtual_memory(virt_addr: usize, layout: Layout) -> Result<(), KError> {
    assert!(IS_ADDR_SPACE_INIT.load(Ordering::Relaxed));

    let kern_addr_space = get_kernel_addr_space();
    unsafe {
        (*kern_addr_space.as_ptr()).lock().reserve_virtual_space(virt_addr, layout)
    }
}

// Flags => VIRTUAL = allocate virtual memory + phy memory + map to current virtual address space
// Flags => NO_ALLOC = Reserve some space in the virtual address space
pub fn allocate_memory(layout: Layout, flags: u8) -> Result<*mut u8, KError> {
    if IS_ADDR_SPACE_INIT.load(Ordering::Relaxed) && (flags & PageDescriptor::VIRTUAL != 0) {
        if flags & PageDescriptor::USER != 0 {
            // If user memory is requested, we don't need to map it into all the address spaces
            // Hence just allocate it in the requested address space
            let active_addr_space = get_active_vcb();
            let virt_addr = unsafe {
                (*active_addr_space.as_ptr()).lock().allocate(layout, true)?
            };
            
            if flags & PageDescriptor::NO_ALLOC == 0 {
                let phy_addr = PHY_MEM_CB.get().unwrap().lock().allocate(layout)?;

                unsafe {&*active_addr_space.as_ptr()}
                .lock().map_memory(phy_addr.addr(), virt_addr.addr(), layout.size(), flags, false)?;
            } 

            
            Ok(virt_addr as *mut u8)
        }
        else {
            // For kernel memory, all address spaces must have same mapping
            // So, we will have the virtual allocation done only on one VCB
            // The rest of them simply will map it in their corresponding page tables

            // Allocate the virtual address from kernel address space.
            let kern_addr_space = get_kernel_addr_space();

            // First, reserve space in virtual address space
            let virt_addr = unsafe {
                (*kern_addr_space.as_ptr()).lock().allocate(layout, false)?
            };

            if flags & PageDescriptor::NO_ALLOC == 0 {
                let phy_addr = PHY_MEM_CB.get().unwrap().lock().allocate(layout)?;                
                let active_addr_space = get_active_vcb();
                // This call is for registering the mapping with the control structures
                if kern_addr_space.as_ptr() != active_addr_space.as_ptr() {
                    unsafe {
                        (*kern_addr_space.as_ptr()).lock().map_memory(
                        phy_addr.addr(),
                        virt_addr.addr(),
                        layout.size(),
                        flags,
                        true)?;
                    }
                }

                // Now map that memory into all address spaces
                unsafe {
                    (*active_addr_space.as_ptr())
                    .lock()
                    .map_memory(phy_addr.addr(), virt_addr.addr(), layout.size(), flags, false)?;
                } 
            }
            
            Ok(virt_addr as *mut u8)
        }
        
    }
    else {
        assert!(flags == 0);
        // Perform only physical allocation
        PHY_MEM_CB.get().unwrap().lock().allocate(layout)
    }
}


// It is important to provide same flags that were provided to allocate_memory for this address
pub fn deallocate_memory(addr: *mut u8, layout: Layout, flags: u8) -> Result<(), KError> {
    if IS_ADDR_SPACE_INIT.load(Ordering::Relaxed) & (flags & PageDescriptor::VIRTUAL != 0) {
        let phy_addr = if flags & PageDescriptor::USER != 0 {
            let active_addr_space = get_active_vcb();

            let phy_addr = if flags & PageDescriptor::NO_ALLOC == 0 {
                unsafe {
                    (*active_addr_space.as_ptr())
                    .lock()
                    .unmap_memory(addr as usize, layout.size(), true, false)?
                }
            } 
            else {
                0 as *mut u8
            };
            
            unsafe {
                (*active_addr_space.as_ptr())
                .lock()
                .deallocate(addr, layout)?
            };

            if flags & PageDescriptor::NO_ALLOC == 0 {
                PageMapper::invalidate_other_cores(MemoryRegion{base_address: addr.addr(), size: layout.size()});
            }

            phy_addr
        }
        else {
            // Deallocate the virtual address from kernel address space.
            let kern_addr_space = get_kernel_addr_space();
            
            // Unmap this memory from all address spaces
            let mut phy_addr = null_mut();
            if flags & PageDescriptor::NO_ALLOC == 0 {
                let active_addr_space = get_active_vcb();
                
                // This call is for registering the mapping with the control structures
                if kern_addr_space.as_ptr() != active_addr_space.as_ptr() {
                    phy_addr = unsafe {
                        (*kern_addr_space.as_ptr()).lock().unmap_memory(
                        addr.addr(),
                        layout.size(),
                        false,
                        true)?
                    };
                    
                    unsafe {
                        (*active_addr_space.as_ptr())
                        .lock()
                        .unmap_memory(addr.addr(), layout.size(), false, true)?;
                    }
                }
                else {
                    phy_addr = unsafe {
                        (*active_addr_space.as_ptr())
                        .lock()
                        .unmap_memory(addr.addr(), layout.size(), false, false)?
                    };
                }
                
                PageMapper::invalidate_other_cores(MemoryRegion{base_address: addr.addr(), size: layout.size()});
            }

            // Unreserve the virtual address from the kernel address space
            unsafe {
                (*kern_addr_space.as_ptr()).lock().deallocate(addr, layout)?;
            };

            phy_addr
        };

        if flags & PageDescriptor::NO_ALLOC == 0 {
            PHY_MEM_CB.get().unwrap().lock().deallocate(phy_addr, layout)?;
        }
        
        Ok(())
    }
    else {
        assert!(flags == 0);
        PHY_MEM_CB.get().unwrap().lock().deallocate(addr, layout)
    }
}


// This is to be called only on virtual address that has been allocated with NO_ALLOC
// Here, the user is responsible for the physical memory
pub fn map_memory(phys_addr: usize, virt_addr: usize, size: usize, flags: u8) -> Result<(), KError> {
    if likely(IS_ADDR_SPACE_INIT.load(Ordering::Relaxed)) {
        let active_addr_space = get_active_vcb();
        let kernel_addr_space = get_kernel_addr_space();
        unsafe {
            // In case the current active address space is not the kernel address space
            // we first modify the control structures (alloc and free block list) which 
            // is only present in kernel address space (for kernel half of memory)
            // Then we do the actual mapping (modify page tables) by calling the mapper in current active address space
            if flags & PageDescriptor::USER == 0 && kernel_addr_space.as_ptr() != active_addr_space.as_ptr() {
                (*kernel_addr_space.as_ptr())
                .lock()
                .map_memory(phys_addr, virt_addr, size, flags, true)?;
            }

            (*active_addr_space.as_ptr())
            .lock()
            .map_memory(phys_addr, virt_addr, size, flags, false)?;
        }
    }
    else {
        // Identity mapped, don't do anything
    }
    
    Ok(())
}

// Only to be called when memory has been previously mapped using map_memory
pub fn unmap_memory(virt_addr: usize, size: usize, flags: u8) -> Result<(), KError> {
    assert!(flags & PageDescriptor::USER == 0);

    if likely(IS_ADDR_SPACE_INIT.load(Ordering::Relaxed)) {
        let active_addr_space = get_active_vcb();
        let kernel_addr_space = get_kernel_addr_space();
        unsafe {
            if flags & PageDescriptor::USER == 0 && kernel_addr_space.as_ptr() != active_addr_space.as_ptr() {
                (*kernel_addr_space.as_ptr())
                .lock()
                .unmap_memory(virt_addr, size, false, true)?;
            }


            (*active_addr_space.as_ptr())
            .lock()
            .unmap_memory(virt_addr, size, flags & PageDescriptor::USER != 0, false)?;
        }
        PageMapper::invalidate_other_cores(MemoryRegion{base_address: virt_addr, size});
    }
    else {
        // Identity mapped, don't do anything
    }
    
    Ok(())
}


pub fn copy_from_user(kernel_dest: *mut u8, user_src: usize, len: usize) -> Result<(), KError> {
    if len == 0 {
        return Ok(());
    }

    let active_addr_space = get_active_vcb();
    unsafe {
        let vcb = (*active_addr_space.as_ptr()).lock();
        if !check_user_range_locked(&vcb, user_src, len) {
            return Err(KError::InvalidArgument);
        }
        hal::copy_user_memory(kernel_dest, user_src as *const u8, len);
    }
    Ok(())
}

pub fn copy_to_user(user_dest: usize, kernel_src: *const u8, len: usize) -> Result<(), KError> {
    if len == 0 {
        return Ok(());
    }

    let active_addr_space = get_active_vcb();
    unsafe {
        let vcb = (*active_addr_space.as_ptr()).lock();
        if !check_user_range_locked(&vcb, user_dest, len) {
            return Err(KError::InvalidArgument);
        }
        hal::copy_user_memory(user_dest as *mut u8, kernel_src, len);
    }
    Ok(())
}

fn check_user_range_locked(vcb: &crate::sync::SpinlockGuard<'_, VirtMemConBlk>, virt_addr: usize, size: usize) -> bool {
    let mut cur_ptr = virt_addr;
    let end = virt_addr + size;
    let mut is_found = true;
    while cur_ptr < end && is_found {
        is_found = false;
        for allocated_node in vcb.alloc_block_list.iter() {
            if allocated_node.is_mapped &&
            (allocated_node.flags & PageDescriptor::USER != 0) &&
            cur_ptr >= allocated_node.start_virt_address &&
            cur_ptr < allocated_node.start_virt_address + allocated_node.num_pages * PAGE_SIZE {
                cur_ptr += allocated_node.num_pages * PAGE_SIZE;
                is_found = true;
            }
        }
    }
    cur_ptr >= end
}

pub fn get_physical_address(virt_addr: usize, flags: u8) -> Option<usize> {
    if IS_ADDR_SPACE_INIT.load(Ordering::Relaxed) {
        if flags & PageDescriptor::USER != 0 {
            let active_addr_space = get_active_vcb();
            unsafe {
                (*active_addr_space.as_ptr()).lock().get_phys_address(virt_addr)
            }
        }
        else {
            let kern_addr_space = get_kernel_addr_space();
            unsafe {
                (*kern_addr_space.as_ptr()).lock().get_phys_address(virt_addr)
            }
        }
    }
    else {
        // Since virtual_mem = physical_mem
        Some(virt_addr)
    }
}

// Unlike get_physical_address, multiple virtual addresses could be mapped to the same physical address
// fetch type allows user to filter out the particular region they want
// Following rules are applicable only when there is more than one virtual address for given physical address
// Kernel -> If present, fetch the lowest address that is > KERNEL_HALF_OFFSET
// Any -> Fetch the lowest virtual address region
pub fn get_virtual_address(phys_addr: usize, flags: u8, fetch_type: MapFetchType) -> Option<usize> {
    if IS_ADDR_SPACE_INIT.load(Ordering::Relaxed) {
        if flags & PageDescriptor::USER != 0 {
            let active_addr_space = get_active_vcb();
            unsafe {
                (*active_addr_space.as_ptr()).lock().get_virt_address(phys_addr, fetch_type)
            }
        }
        else {
            let kern_addr_space = get_kernel_addr_space();
            unsafe {
                (*kern_addr_space.as_ptr()).lock().get_virt_address(phys_addr, fetch_type)
            } 
        }
    }
    else {
        // Since virtual_mem = physical_mem
        Some(hal::canonicalize_virtual(phys_addr))
    }
}

pub fn virtual_allocator_init() {
    // Create the kernel address space and attach it to first node in address space list
    let remap_list = REMAP_LIST.lock();

    // All page tables that are mapped must be below 4GB. This is to later support MP init
#[cfg(target_arch = "x86_64")] 
    {
        PHY_MEM_CB.get().unwrap().lock().configure_upper_limit((1 << 32) - 1); // 4GB
        PHY_MEM_CB.get().unwrap().lock().configure_lower_limit(1 << 20); // 1MB
    }

    let mut kernel_addr_space = VirtMemConBlk::new(true);
    let dummy_reserve = [0; 4];
    // First map the identity mapped regions
    // In case, identity mapped region straddles the kernel upper half, the checks within function will halt kernel
    // We can take it up later
    remap_list.iter().filter(|item| {
        matches!(item.map_type, IdentityMapped)
    }).for_each(|item| {
        info!("Identity mapping region of size:{} with physical address:{:#X}", 
        item.value.size, item.value.base_address);
        let layout = Layout::from_size_align(item.value.size, PAGE_SIZE).unwrap();
        kernel_addr_space.reserve_virtual_space(item.value.base_address, layout)
        .expect("Failed to reserve identity mapped address in kernel virtual address space!");

        kernel_addr_space.map_memory(
            item.value.base_address, item.value.base_address, 
            item.value.size, item.flags, true).unwrap();

        kernel_addr_space.page_mapper.map_memory_non_self(
            &dummy_reserve,
            item.value.base_address,
            item.value.base_address,
            item.value.size,
            item.flags
        );
    });

    // Now map remaining set of regions onto upper half of memory
    remap_list.iter().filter(|item| {
        !matches!(item.map_type, IdentityMapped)
    }).for_each(|item| {
        let layout = Layout::from_size_align(item.value.size, PAGE_SIZE).unwrap();
        let virt_addr = kernel_addr_space.allocate(layout, false)
        .expect("System could not find suitable memory in higher half kernel space");
        
        info!("Mapping region of size:{} with physical address:{:#X} to virtual address:{:#X}", 
        item.value.size, item.value.base_address, virt_addr as usize);

        kernel_addr_space.map_memory(
            item.value.base_address,
            virt_addr as usize, 
            item.value.size, 
            item.flags, 
            true).unwrap();
        
        kernel_addr_space.page_mapper.map_memory_non_self(
            &dummy_reserve,
            virt_addr as usize,
            item.value.base_address,
            item.value.size,
            item.flags
        );
        
        // Update user of new location
        if let OffsetMapped(f) = &item.map_type {
            f(virt_addr as usize);
        }
    });
    
    // Create a new stack for boot cpu
    let stack_raw= kernel_addr_space.allocate(Layout::from_size_align(cpu::TOTAL_BOOT_STACK_SIZE, PAGE_SIZE).unwrap()
    , false).expect("Failed to create space in virtual address for boot cpu stack");

    let stack_raw_phys = PHY_MEM_CB.get().unwrap().lock().allocate(Layout::from_size_align(cpu::INIT_BOOT_CPU_STACK_SIZE, PAGE_SIZE).unwrap())
    .expect("Failed to create space for physical address space for boot cpu stack");

    #[cfg(feature = "stack_down")]
    let stack_base = unsafe {
        stack_raw.add(cpu::INIT_GUARD_PAGE_SIZE)
    };

    #[cfg(not(feature = "stack_down"))]
    let stack_base = stack_raw;

    kernel_addr_space.map_memory(
        stack_raw_phys.addr(), 
        stack_base.addr(), 
        cpu::INIT_BOOT_CPU_STACK_SIZE, 
        0, 
        true)
    .expect("Failed to map boot cpu stack to kernel virtual address space!");
    
    kernel_addr_space.page_mapper.map_memory_non_self(
        &dummy_reserve,
        stack_base.addr(),
        stack_raw_phys.addr(),
        cpu::INIT_BOOT_CPU_STACK_SIZE,
        0
    );

    debug!("Created boot cpu stack with virtual address: {:#X} and physical address: {:#X}", stack_base.addr(), stack_raw_phys.addr());

    cpu::set_worker_stack_for_boot_cpu(stack_raw);

    // Finalize address space creation
    kernel_addr_space.page_mapper.set_allocated();

    ADDRESS_SPACES.call_once(|| {
        let mut l = List::new();
        l.add_node(Spinlock::new(kernel_addr_space)).unwrap();

        Spinlock::new(l)
    }); 

    // The pointer referenced here will never be moved (Atleast, not the kernel base address space (The first address space))
    PER_CPU_ACTIVE_VCB.local().store(
        ptr_to_ref_mut(&**ADDRESS_SPACES.get().unwrap().lock().first().unwrap()),
        Ordering::Release
    );   

    KERN_ADDR_SPACE.call_once(|| {
        AtomicPtr::new(ptr_to_ref_mut(&**ADDRESS_SPACES.get().unwrap().lock().first().unwrap()))
    });

    IS_ADDR_SPACE_INIT.store(true, Ordering::SeqCst);

    info!("Created kernel address space"); 
}

#[cfg(test)]
pub fn virtual_allocator_test() {
    let mut allocator = VirtMemConBlk::new(true);

    // Check allocating from user memory
    let layout = Layout::from_size_align(10 * PAGE_SIZE, 4096).unwrap();
    let ptr= allocator.allocate(layout, true).unwrap();

    assert_eq!(ptr as usize, 4096);
    assert!(allocator.free_block_list.get_nodes() == 2 && allocator.free_block_list.first().unwrap().start_virt_address == 11 * PAGE_SIZE);

    let ptr1 = allocator.allocate(layout, true).unwrap();
    assert_eq!(ptr1 as usize, 11 * PAGE_SIZE);

    let ptr2 = allocator.allocate(layout, true).unwrap();
    assert_eq!(ptr2 as usize, 21 * PAGE_SIZE);

    allocator.deallocate(ptr1, layout).unwrap();
    assert_eq!(allocator.free_block_list.get_nodes(), 3);    
    let nodes = [31 * PAGE_SIZE, KERNEL_HALF_OFFSET, 11 * common::PAGE_SIZE];
    allocator.free_block_list.iter().zip(nodes).for_each(|(blk, address)| {
        assert_eq!(blk.start_virt_address, address);
    });

    // Check coalescing
    allocator.deallocate(ptr, layout).unwrap();
    assert_eq!(allocator.free_block_list.get_nodes(), 3);
    let nodes = [31 * PAGE_SIZE, KERNEL_HALF_OFFSET, common::PAGE_SIZE];
    allocator.free_block_list.iter().zip(nodes).for_each(|(blk, address)| {
        assert_eq!(blk.start_virt_address, address);
    });

    assert!(allocator.deallocate(ptr1, layout).is_err_and(|e| {
        e == KError::InvalidArgument
    }));

    allocator.deallocate(ptr2, layout).unwrap();
    assert_eq!(allocator.free_block_list.get_nodes(), 2);
    
    let nodes = [KERNEL_HALF_OFFSET, PAGE_SIZE];
    allocator.free_block_list.iter().zip(nodes).for_each(|(blk, address)| {
        assert_eq!(blk.start_virt_address, address);
    });

    // Try allocating from kernel memory and checking
    let ptr = allocator.allocate(layout, false).unwrap();
    assert_eq!(ptr as usize, KERNEL_HALF_OFFSET);

    let ptr1 = allocator.allocate(layout, false).unwrap();
    assert_eq!(ptr1 as usize, KERNEL_HALF_OFFSET + 10 * PAGE_SIZE);
    assert_eq!(allocator.free_block_list.get_nodes(), 2);
    
    let nodes = [KERNEL_HALF_OFFSET + 20 * PAGE_SIZE, common::PAGE_SIZE];
    allocator.free_block_list.iter().zip(nodes).for_each(|(blk, address)| {
        assert_eq!(blk.start_virt_address, address);
    });
    
    allocator.deallocate(ptr, layout).unwrap();
    assert_eq!(allocator.free_block_list.get_nodes(), 3);
    let nodes = [KERNEL_HALF_OFFSET + 20 * PAGE_SIZE, common::PAGE_SIZE, KERNEL_HALF_OFFSET];
    allocator.free_block_list.iter().zip(nodes).for_each(|(blk, address)| {
        assert_eq!(blk.start_virt_address, address);
    });

    // Back to square 1
    allocator.deallocate(ptr1, layout).unwrap();
    assert_eq!(allocator.free_block_list.get_nodes(), 2);
    let nodes = [PAGE_SIZE, KERNEL_HALF_OFFSET];
    allocator.free_block_list.iter().zip(nodes).for_each(|(blk, address)| {
        assert_eq!(blk.start_virt_address, address);
    });
}

pub fn on_page_fault(fault_address: usize) {
    try_kill_user_process(PAGE_FAULT_VECTOR, "Page fault");
    panic!("Page fault exception!\nFault address:{:#X}", fault_address);
}