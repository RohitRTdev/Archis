use common::{MemType, MemoryDesc, PAGE_SIZE};
use crate::{RemapEntry, RemapType::*, BOOT_INFO, REMAP_LIST};
use crate::mem::FixedList;
use kernel_intf::list::{List, ListNode};
use crate::sync::{Once, Spinlock};
use kernel_intf::KError;
use kernel_intf::info;
use super::Regions::*;
use super::PageDescriptor;
use core::alloc::Layout;
use core::ptr::NonNull;

#[cfg(target_arch = "x86_64")]
const ARCH_PHY_UPPER_LIMIT: u64 = 0xffffffffffffffff;

#[cfg(target_arch = "x86_64")]
const ARCH_PHY_LOWER_LIMIT: u64 = 0;

pub struct PhyMemConBlk {
    total_memory: usize,
    avl_memory: usize,
#[cfg(target_arch = "x86_64")]
    hard_limit: u64,
#[cfg(target_arch = "x86_64")]
    lower_limit: u64,
    free_block_list: FixedList<PageDescriptor, {Region0 as usize}>,
    alloc_block_list: FixedList<PageDescriptor, {Region0 as usize}>, 
}

pub static PHY_MEM_CB: Once<Spinlock<PhyMemConBlk>> = Once::new();

static BOOTLOADER_REGIONS: Once<Spinlock<FixedList<PageDescriptor, {Region0 as usize}>>> = Once::new();

impl PhyMemConBlk {
    fn find_best_fit(&mut self, pages: usize) -> Result<*mut u8, KError> {
        let mut smallest_blk: Option<&mut ListNode<PageDescriptor>> = None;

        // Track the block with the smallest number of pages that can satisfy above request
        for block in self.free_block_list.iter_mut() {
            // Also check if it satisfies the upper & lower limit constraints
            if block.num_pages >= pages && 
            block.start_phy_address + pages * PAGE_SIZE - 1 <= self.hard_limit as usize && 
            block.start_phy_address >= self.lower_limit as usize {
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
            let start_address = node.start_phy_address as *mut u8;
            node.start_phy_address += pages * PAGE_SIZE;
            if node.num_pages == 0 {
                let list_node = NonNull::from(node);
                unsafe {
                    self.free_block_list.remove_node(list_node);
                }
            }

            self.alloc_block_list.add_node(PageDescriptor { num_pages: pages, start_phy_address: start_address as usize, 
                start_virt_address: 0x0, flags: 0x0, is_mapped: false })?;

            return Ok(start_address);
        }
        else {
            return Err(KError::OutOfMemory);
        }
    }
    
    #[cfg(target_arch = "x86_64")]
    pub fn configure_lower_limit(&mut self, lower_limit: u64) {
        info!("Configuring frame allocator lower limit:{:#X}", lower_limit);
        self.lower_limit = lower_limit;
    }

    #[cfg(target_arch = "x86_64")]
    pub fn configure_upper_limit(&mut self, upper_limit: u64) {
        info!("Configuring frame allocator upper limit:{:#X}", upper_limit);
        self.hard_limit = upper_limit;
    }
    
    #[allow(dead_code)]
    #[cfg(target_arch = "x86_64")]
    pub fn disable_limits(&mut self) {
        info!("Disabling frame allocator upper and lower limit");
        self.hard_limit = ARCH_PHY_UPPER_LIMIT;
        self.lower_limit = ARCH_PHY_LOWER_LIMIT;
    }

    pub fn allocate(&mut self, layout: Layout) -> Result<*mut u8, KError> {
        if layout.size() >= self.avl_memory {
            info!("Frame allocator our of memory!. Requested: {}, Available: {}", layout.size(), self.avl_memory);
            return Err(KError::OutOfMemory);
        }

        if layout.align() > PAGE_SIZE {
            return Err(KError::InvalidArgument);
        }

        let num_pages = common::ceil_div(layout.size(), PAGE_SIZE);
        let addr = self.find_best_fit(num_pages)?;    
        self.avl_memory -= num_pages * PAGE_SIZE;
        Ok(addr)
    }

    pub fn deallocate(&mut self, addr: *mut u8, layout: Layout) -> Result<(), KError> {
        if layout.align() > PAGE_SIZE {
            return Err(KError::InvalidArgument);
        }

        let num_pages = common::ceil_div(layout.size(), PAGE_SIZE);

        // Remove node from alloc_block_list
        let mut alloc_blk = None;
        for blk in self.alloc_block_list.iter() {
            if blk.start_phy_address == addr as usize && blk.num_pages == num_pages {
                alloc_blk = Some(NonNull::from(blk));
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
            return Err(KError::InvalidArgument);
        } 
        
        let mut found_blk = None; 
        let num_size = num_pages * PAGE_SIZE;
        let addr = addr as usize;
        
        // Check if this block can be merged with an existing block
        for blk in self.free_block_list.iter_mut() {
            if blk.start_phy_address + blk.num_pages * PAGE_SIZE == addr {
                blk.num_pages += num_pages;
                found_blk = Some(NonNull::from(&*blk));
                break;
            }
            else if addr + num_size == blk.start_phy_address {
                blk.start_phy_address -= num_size;
                blk.num_pages += num_pages;
                found_blk = Some(NonNull::from(&*blk));
                break;
            }
        }

        // Now run same algorithm once more (There could be atmost 2 blocks to which a fragmented block could be merged)        
        if let Some(blk) = found_blk {
            let blk_desc = unsafe {blk.as_ref()};
            let merge_blk = self.free_block_list.iter_mut().find(|item| {
                (item.start_phy_address + item.num_pages * PAGE_SIZE == blk_desc.start_phy_address) || 
                (blk_desc.start_phy_address + blk_desc.num_pages * PAGE_SIZE == item.start_phy_address) 
            });

            // We found one more block to which the new block can be merged
            // In this case all three blocks are merged as one
            if let Some(merge_blk_desc) = merge_blk {
                merge_blk_desc.num_pages += blk_desc.num_pages;
                merge_blk_desc.start_phy_address = blk_desc.start_phy_address.min(merge_blk_desc.start_phy_address);
                unsafe {
                    self.free_block_list.remove_node(blk);
                }
            }
        } 
        else {
            // If no block to which the fragmented region can be merged, just create a new block to describe the free region
            // If it fails at this point, it's hard to recover
            self.free_block_list.add_node(PageDescriptor { num_pages, start_phy_address: addr, start_virt_address: 0, flags: 0, is_mapped: false })
            .expect("System in bad state. Critical memory failure!");
        }

        self.avl_memory += num_size;
        Ok(())
    }
}

pub fn get_available_memory() -> usize {
    PHY_MEM_CB.get().unwrap().lock().avl_memory
}

pub fn frame_allocator_init() {
    let boot_info = BOOT_INFO.get().unwrap();
    let mut init_mem_cb = PhyMemConBlk {
        total_memory: 0,
        avl_memory: 0,
        hard_limit: ARCH_PHY_UPPER_LIMIT,
        lower_limit: ARCH_PHY_LOWER_LIMIT,
        free_block_list: List::new(),
        alloc_block_list: List::new()
    };
    let mut bl_list: FixedList<PageDescriptor, {Region0 as usize}> = List::new();

    let mem_descriptors  = unsafe {
        core::slice::from_raw_parts_mut(boot_info.memory_map_desc.start as *mut MemoryDesc, boot_info.memory_map_desc.size / boot_info.memory_map_desc.entry_size)
    };

    for desc in mem_descriptors {
        // Remove page 0 from frame allocation. Since various systems consider 0 as null value,
        // we will not include it
        if desc.val.base_address == 0 {
            desc.val.base_address += PAGE_SIZE;
            if desc.val.size > PAGE_SIZE {
                desc.val.size -= PAGE_SIZE;
            }
            else {
                continue;
            }
        }

        match &desc.mem_type {
            MemType::Free => {
                init_mem_cb.free_block_list.add_node(PageDescriptor { num_pages: common::ceil_div(desc.val.size, PAGE_SIZE),
                    start_phy_address: desc.val.base_address, start_virt_address: 0, flags: 0, is_mapped: false }).unwrap();

                init_mem_cb.avl_memory += desc.val.size;
            },
            MemType::Allocated | MemType::Identity => {
                init_mem_cb.alloc_block_list.add_node(PageDescriptor { num_pages: common::ceil_div(desc.val.size, PAGE_SIZE),
                    start_phy_address: desc.val.base_address, start_virt_address: 0, flags: 0, is_mapped: false }).unwrap();

                if desc.mem_type == MemType::Identity {
                    REMAP_LIST.lock().add_node(RemapEntry {
                        value: desc.val,
                        map_type: IdentityMapped, flags: 0}).unwrap();
                }
            },
            MemType::BootloaderData => {
                // Pages are not free yet; they will be reclaimed in kern_main after
                // all firmware-provided data has been consumed.
                bl_list.add_node(PageDescriptor { num_pages: common::ceil_div(desc.val.size, PAGE_SIZE),
                    start_phy_address: desc.val.base_address, start_virt_address: 0, flags: 0, is_mapped: false }).unwrap();
            },
        }
        init_mem_cb.total_memory += desc.val.size;
    }

    info!("Initialized Memory control block -> Total memory: {}, Available memory: {}", init_mem_cb.total_memory, init_mem_cb.avl_memory);

    PHY_MEM_CB.call_once(|| Spinlock::new(init_mem_cb));
    BOOTLOADER_REGIONS.call_once(|| Spinlock::new(bl_list));
}

pub fn reclaim_pages() {
    let bl_regions = match BOOTLOADER_REGIONS.get() {
        Some(r) => r,
        None => return,
    };

    let mut bl_list = bl_regions.lock();
    let mut phy_mem = PHY_MEM_CB.get().unwrap().lock();

    loop {
        let node_ptr = bl_list.iter().next().map(|n| NonNull::from(n));
        let Some(ptr) = node_ptr else { break };

        let (num_pages, base) = {
            let desc = unsafe { ptr.as_ref() };
            (desc.num_pages, desc.start_phy_address)
        };
        unsafe { bl_list.remove_node(ptr); }

        // Temporarily add to alloc list so deallocate() can locate and coalesce it.
        phy_mem.alloc_block_list.add_node(PageDescriptor {
            num_pages,
            start_phy_address: base,
            start_virt_address: 0,
            flags: 0,
            is_mapped: false,
        }).unwrap();

        let layout = Layout::from_size_align(num_pages * PAGE_SIZE, PAGE_SIZE).unwrap();
        phy_mem.deallocate(base as *mut u8, layout)
            .expect("reclaim_pages: deallocate failed");
    }

    info!("Reclaimed bootloader data pages. Available memory: {}", phy_mem.avl_memory);
}


#[cfg(test)] 
pub fn test_init_allocator() {
    common::test_log!("Initializing physical allocator");
    let desc1 = PageDescriptor {
        num_pages: 10,
        start_phy_address: 0x0,
        start_virt_address: 0x0,
        flags: 0x0,
        is_mapped: false
    };

    let desc2 = PageDescriptor {
        num_pages: 2,
        start_phy_address: 20 * PAGE_SIZE,
        start_virt_address: 0x0,
        flags: 0x0,
        is_mapped: false
    };

    let desc3 = PageDescriptor {
        num_pages: 6,
        start_phy_address: 40 * PAGE_SIZE,
        start_virt_address: 0x0,
        flags: 0x0,
        is_mapped: false
    };
    
    let mut free_block_list= List::new();
    free_block_list.add_node(desc1).unwrap();
    free_block_list.add_node(desc2).unwrap();
    free_block_list.add_node(desc3).unwrap();

    let cb = PhyMemConBlk {
        total_memory: 18 * PAGE_SIZE,
        avl_memory: 18 * PAGE_SIZE,
        hard_limit: ARCH_PHY_UPPER_LIMIT,
        lower_limit: ARCH_PHY_LOWER_LIMIT,
        free_block_list,
        alloc_block_list: List::new()
    };

    PHY_MEM_CB.call_once(|| {
        Spinlock::new(cb)
    });
}

#[cfg(test)]
pub fn check_mem_nodes() {

    // We should have (8) - (2 + 6 + 2) layout
    let allocator = PHY_MEM_CB.get().unwrap().lock();

    assert_eq!(allocator.free_block_list.get_nodes(), 1);
    assert_eq!(allocator.alloc_block_list.get_nodes(), 3);

    let free_list = [8];
    let alloc_list = [2, 6, 2];

    common::test_log!("Printing free_block_list....");
    for (idx, blk) in allocator.free_block_list.iter().enumerate() {
        assert_eq!(free_list[idx], blk.num_pages);
        common::test_log!("{:?}", **blk);
    }
    
    common::test_log!("Printing alloc_block_list....");
    for (idx, blk) in allocator.alloc_block_list.iter().enumerate() {
        assert_eq!(alloc_list[idx], blk.num_pages);
        common::test_log!("{:?}", **blk);
    }
}
