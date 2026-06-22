use core::sync::atomic::{AtomicIsize, AtomicUsize, Ordering};
use core::alloc::Layout;
use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};
use kernel_intf::mem::PoolAllocatorGlobal;
use common::{elf::*, PAGE_SIZE};
use common::{MemoryRegion, ModuleInfo, FileDescriptor};
use crate::fs::FileInstance;
use crate::loader::LoadedImage;
use crate::{BOOT_INFO, InitFS, KERNEL_PATH, REMAP_LIST, RemapEntry, RemapType::*};
use crate::sync::{Once, Spinlock};
use kernel_intf::{info, debug};
use crate::mem::{self, MapFetchType, PageDescriptor};

#[derive(Clone)]
pub struct KernelModule {
    pub name: &'static str,
    pub driver_init_address: Option<usize>,
    pub driver_unload_address: Option<usize>,
    pub file_handle: Option<FileInstance>,
    pub info: ModuleInfo,

    // This is here so that the dependencies are not released when this image is loaded
    pub _deps: Option<Vec<LoadedImage>>
}

#[derive(Clone)]
pub struct UserModuleSegment {
    pub phys_addr: usize,
    pub offset: usize,
    pub size: usize,
    pub writable: bool
}

pub type SharedUserModuleRef = Arc<Spinlock<SharedUserModule>, PoolAllocatorGlobal>;

pub struct SharedUserModule {
    pub file_handle: FileInstance,
    pub segments: Vec<UserModuleSegment>,
    pub total_size: usize,
    pub entry_offset: usize,

    // Module-relative locations of the dynamic symbol data (lives inside a read
    // segment). Used to resolve imports from dependents via temporary kernel mappings.
    pub dyn_tab_off: Option<usize>,
    pub num_syms: usize,
    pub dyn_str_off: Option<usize>,
    pub dyn_str_size: usize,

    // Keeps the shared descriptors of this module's dependencies alive
    pub _deps: Vec<SharedUserModuleRef>
}

#[derive(Clone)]
pub struct UserModule {
    pub base: usize,
    pub entry: usize,
    pub shared: SharedUserModuleRef
}

#[derive(Clone)]
pub enum ModuleType {
    Kernel(KernelModule),
    User(UserModule)
}

#[derive(Clone)]
pub struct ModuleDescriptor {
    pub mod_type: ModuleType
}

impl ModuleDescriptor {
    pub fn kernel(&self) -> &KernelModule {
        match &self.mod_type {
            ModuleType::Kernel(k) => k,
            ModuleType::User(_) => panic!("Expected kernel module descriptor!")
        }
    }

    pub fn kernel_mut(&mut self) -> &mut KernelModule {
        match &mut self.mod_type {
            ModuleType::Kernel(k) => k,
            ModuleType::User(_) => panic!("Expected kernel module descriptor!")
        }
    }

    pub fn kernel_opt(&self) -> Option<&KernelModule> {
        match &self.mod_type {
            ModuleType::Kernel(k) => Some(k),
            ModuleType::User(_) => None
        }
    }

    pub fn user(&self) -> &UserModule {
        match &self.mod_type {
            ModuleType::User(u) => u,
            ModuleType::Kernel(_) => panic!("Expected user module descriptor!")
        }
    }
}

pub static ARIS: Once<Spinlock<ModuleDescriptor>> = Once::new(); 

static FILE_INDEX: AtomicUsize = AtomicUsize::new(0);
static KERNEL_OFFSET: AtomicIsize = AtomicIsize::new(0);

pub fn early_init() {
    let info = BOOT_INFO.get().unwrap();
    let kernel_base_address = info.kernel_desc.base;  
    let kernel_total_size = info.kernel_desc.total_size; 
    let mod_cb = ModuleDescriptor {
        mod_type: ModuleType::Kernel(KernelModule {
            name: env!("CARGO_PKG_NAME"),
            driver_init_address: None,
            driver_unload_address: None,
            file_handle: None,
            info: info.kernel_desc,
            _deps: None
        })
    };
    
    // Map the kernel and auxiliary tables onto upper half
    let mut remap_list = REMAP_LIST.lock();
    remap_list.add_node(RemapEntry {
        value: MemoryRegion {
            base_address: info.kernel_desc.base,
            size: info.kernel_desc.total_size
        },
        map_type: OffsetMapped(|kern_base| {
            let mut mod_cb = ARIS.get().unwrap().lock();
            let kmod = mod_cb.kernel_mut();
            let offset = kern_base as isize - kmod.info.base as isize;
            KERNEL_OFFSET.store(offset, Ordering::Release);
            let add_offset = |a: usize| {
                (a as isize + offset) as usize
            };

            kmod.info.base = kern_base;
            kmod.info.entry = add_offset(kmod.info.entry);

            if let Some(val) = &mut kmod.info.sym_tab {
                val.start = add_offset(val.start);
            }
            if let Some(val) = &mut kmod.info.sym_str {
                val.base_address = add_offset(val.base_address);
            }
            if let Some(val) = &mut kmod.info.dyn_tab {
                val.start = add_offset(val.start);
            }
            if let Some(val) = &mut kmod.info.dyn_shn {
                val.start = add_offset(val.start);
            }
            if let Some(val) = &mut kmod.info.rlc_shn {
                val.start = add_offset(val.start);
            }
            if let Some(val) = &mut kmod.info.plt_shn {
                val.start = add_offset(val.start);
            }
            if let Some(val) = &mut kmod.info.dyn_str {
                val.base_address = add_offset(val.base_address);
            }

            crate::loader_log!("Updated kernel module info = {:?}", kmod.info);
        }),
        flags: 0
    }).unwrap();

    // Relocate init fs
    let fs_entries = unsafe {
        core::slice::from_raw_parts_mut(info.init_fs.start as *mut FileDescriptor, info.init_fs.size / info.init_fs.entry_size)
    };

    for entry in fs_entries {
        assert!(entry.contents.as_ptr() as usize & (PAGE_SIZE - 1) == 0);
        remap_list.add_node(RemapEntry { 
            value: MemoryRegion { 
                base_address: entry.contents.as_ptr() as usize,
                size: entry.contents.len() + entry.name.len()
            },
            map_type: OffsetMapped(|virt_addr| {
                let info = BOOT_INFO.get().unwrap();
                let fs_entries = unsafe {
                    core::slice::from_raw_parts_mut(info.init_fs.start as *mut FileDescriptor, info.init_fs.size / info.init_fs.entry_size)
                };


                let entry = &mut fs_entries[FILE_INDEX.fetch_add(1, Ordering::Relaxed)]; 
                entry.contents = unsafe {
                    core::slice::from_raw_parts(virt_addr as *const u8, entry.contents.len())
                };
                
                entry.name = unsafe {
                    let ptr = core::slice::from_raw_parts((virt_addr + entry.contents.len()) as *const u8, entry.name.len());
                    core::str::from_utf8_unchecked(ptr)
                };

            }),
            flags: 0
        }).unwrap();
    }

    // Identity map the descriptors pointing to the files 
    remap_list.add_node(RemapEntry { 
        value: MemoryRegion { 
            base_address: info.init_fs.start, 
            size: info.init_fs.size
        }, 
        map_type: IdentityMapped,
        flags: 0
    }).unwrap();

    // ID map the kernel
    // We will not remove this mapping after address transition in order to avoid problems due to any cached 
    // addresses (Say, due to compiler optimization) and to support fixed allocator entries
    remap_list.add_node(RemapEntry {
        value: MemoryRegion {
            base_address: kernel_base_address,
            size: kernel_total_size
        },
        map_type: IdentityMapped,
        flags: 0
    }).unwrap();


    ARIS.call_once(|| {
        Spinlock::new(mod_cb)
    });
}


pub fn complete_handoff() {
    info!("Reapplying relocations to switch to new address space");
    let mut mod_cb = ARIS.get().unwrap().lock();
    let boot_info = BOOT_INFO.get().unwrap();
    let koffset = KERNEL_OFFSET.load(Ordering::Acquire);
    
    // This is the old unmapped kernel address
    let load_base = mod_cb.kernel().info.base;

    let info = |bitmap: u64| {
        (bitmap & 0xffffffff) as u32
    };

    if let Some(rlc_shn) = &mod_cb.kernel().info.rlc_shn {
        // Usually we separate between plt and rlc relocations.
        // However, kernel is known to have lot of relocations, so the reloc section
        // will definitely exist and if it does, it will also contain the JUMP_SLOT plt entries
        // We process them both from the same table unlike other parts within the kernel and blr
        let num_rel_entries = rlc_shn.size / core::mem::size_of::<Elf64Rela>();
        let entries = unsafe {
            core::slice::from_raw_parts(rlc_shn.start as *const Elf64Rela, num_rel_entries)
        };
        
        for entry in entries {
            let address = load_base + entry.r_offset as usize;
            match info(entry.r_info) {
                R_X86_64_RELATIVE | R_X86_64_64 | 
                R_GLOB_DAT | R_JUMP_SLOT => {
                    // This is not normal relocation. We might have made changes
                    // to various relocation pointers at runtime.
                    // So we just need to add the offset that we have moved the kernel into it
                    unsafe {
                        let cur_address = *(address as *mut u64);
                        *(address as *mut u64) = (koffset + cur_address as isize) as u64;
                    };
                },
                _ => {} 
            }
        }
    }
    
    // The module name needs to be patched up to new address
    let name_len = mod_cb.kernel().name.len();
    let name_ptr = mem::get_virtual_address(mod_cb.kernel().name.as_ptr() as usize, 0,  MapFetchType::Kernel)
    .expect("Unable to find virtual address for module name");

    mod_cb.kernel_mut().name = unsafe {
        let slice = core::slice::from_raw_parts(name_ptr as *const u8, name_len);
        core::str::from_utf8_unchecked(slice)
    };

    kernel_intf::set_logger_name(mod_cb.kernel().name);

    debug!("Module address:{:#X}, mod_name={}", mod_cb.kernel().name.as_ptr() as usize, mod_cb.kernel().name);

    // Reconstruct init fs as a hashmap. This is done here, since we now have access to heap
    crate::INIT_FS.call_once(|| {
        let boot_info = BOOT_INFO.get().unwrap();
        let fs_entries = unsafe {
            core::slice::from_raw_parts(boot_info.init_fs.start as *const FileDescriptor, boot_info.init_fs.size / boot_info.init_fs.entry_size) 
        };

        let mut map = BTreeMap::new();

        for entry in fs_entries {
            info!("Adding init fs entry:{} with start_addr:{:#X}", entry.name, entry.contents.as_ptr().addr());
            map.insert(entry.name, entry.contents);
        }

        let mut symlinks = BTreeMap::new();
        symlinks.insert("/sys/libaris.so", KERNEL_PATH);

        InitFS {
            fs: map,
            symlinks
        }
    });

    info!("Init-FS address:{:#X}, num_files={}", crate::INIT_FS.get().unwrap() as *const _ as usize, crate::INIT_FS.get().unwrap().fs.len());
    
    // We have moved the init-fs metadata into kernel binary
    // So we can remove the descriptors we had
    // We can't call mem::deallocate_memory with VIRTUAL flag alone as the physical memory was allocated by blr. So we just unmap instead
    debug!("Deallocating init_fs at start: {:#X} with size: {}", boot_info.init_fs.start, boot_info.init_fs.size);
    mem::unmap_memory(boot_info.init_fs.start, boot_info.init_fs.size, 0).expect("Could not deallocate init-fs descriptor memory");
    mem::deallocate_memory(boot_info.init_fs.start as *mut u8, Layout::from_size_align(boot_info.init_fs.size, PAGE_SIZE).unwrap(), PageDescriptor::VIRTUAL | PageDescriptor::NO_ALLOC)
    .expect("Unable to unreserve virtual address space for init fs");
    info!("Handoff procedure completed");
}