use alloc::sync::{Arc, Weak};
use alloc::format;
use alloc::vec::Vec;
use alloc::string::String;
use alloc::borrow::ToOwned;
use kernel_intf::KError::InvalidArgument;
use core::alloc::Layout;
use core::mem::{size_of, align_of};
use core::ptr::copy_nonoverlapping;
use core::ffi::CStr;
use common::{PAGE_SIZE, elf::*};
use common::{ArrayTable, MemoryRegion, ModuleInfo, StrRef};
use kernel_intf::{KError, info};
use crate::KERNEL_PATH;
use crate::fs::{FileBuffer, open, resolve_symlink};
use crate::infra::disable_preloader_phase;
use crate::loader::module::ModuleDescriptor;
use crate::mem::{PageDescriptor, allocate_memory, deallocate_memory};
use kernel_intf::mem::PoolAllocatorGlobal;
use crate::sched::Handle::ImgHandle;
use crate::sched::add_new_handle;
use crate::sync::{KSem, Once, Spinlock, semaphore_guard};
use kernel_intf::list::{List, DynList};
use super::module;

pub static KERNEL_MODULES: Spinlock<DynList<Weak<Spinlock<ModuleDescriptor>, PoolAllocatorGlobal>>> = Spinlock::new(List::new());

static LOAD_LOCK: Once<KSem> = Once::new();

pub type LoadedImage = Arc<Spinlock<ModuleDescriptor>, PoolAllocatorGlobal>;

impl Drop for ModuleDescriptor {
    fn drop(&mut self) {
        {
            let mut registry = KERNEL_MODULES.lock();
            info!("Dropping image {}, with registry_len={}", self.name, registry.get_nodes());

            // Cleanup: Remove all weak refs from the registry
            while registry.find_and_remove(|entry| {
                let item = entry.upgrade();
                item.is_none()
            }).is_some() {}

            info!("New registry len = {}", registry.get_nodes());
        }

        deallocate_memory(
            self.info.base as *mut u8, 
            Layout::from_size_align(self.info.total_size, PAGE_SIZE).unwrap(), 
            PageDescriptor::VIRTUAL
        ).expect("Failed to deallocate backing memory for kernel module!");
    }
}


pub fn init() {
    LOAD_LOCK.call_once(|| KSem::new(1, 1));

    let mut kernel_img = module::ARIS.get().unwrap().lock().clone();
    kernel_img.file_handle = Some(
        open(KERNEL_PATH).expect("Failed to open kernel image!")
    );

    let loaded_img = Arc::new_in(
        Spinlock::new(
            kernel_img
        ),
        PoolAllocatorGlobal
    );

    let downgraded_ref = Arc::downgrade(&loaded_img);

    add_new_handle(ImgHandle(loaded_img));
    KERNEL_MODULES.lock().add_node(downgraded_ref)
    .expect("Failed to add kernel image module to Loaded images registry!");

    disable_preloader_phase();
}

pub fn load_image(path: &str, is_user: bool) -> Result<LoadedImage, KError> {
    info!("Start load_image for {}", path);

    // Serialize the whole recursive load
    let _guard = semaphore_guard(LOAD_LOCK.get().expect("loader::init() not called before load_image()"));

    let mut in_progress: Vec<String> = Vec::new();
    let result = load_image_inner(path, is_user, &mut in_progress);

    result
}

fn load_image_inner(
    path: &str,
    is_user: bool,
    in_progress: &mut Vec<String>
) -> Result<LoadedImage, KError> {
    if is_user {
        todo!("User-mode image loading not implemented");
    }

    if let Some(cached) = find_loaded_module(path) {
        info!("Loading image {} from cache", path);
        return Ok(cached);
    }

    if in_progress.iter().any(|n| n == path) {
        return Err(KError::CircularDependency);
    }
    in_progress.push(path.to_owned());

    let result = load_image_uncached(
        path,
        is_user,
        in_progress
    );

    in_progress.pop();
    result
}

fn load_image_uncached(
    path: &str,
    is_user: bool,
    in_progress: &mut Vec<String>
) -> Result<LoadedImage, KError> {
    info!("Loading image {} from disk", path);
    let file = open(path)?;
    let file_size = file.lock().len();

    let buf= FileBuffer::new(file_size, false)
    .or_else(|e| {
        info!("Failed to allocate filebuffer for image {}", path);
        Err(e)
    })?;

    let read_len = file.lock().read(&buf);
    if read_len != file_size {
        info!("read_len={} doesn't seem to match file_size={} for path={}", read_len, file_size, path);
        return Err(KError::InvalidArgument);
    }
    let bytes = buf.as_slice();

    let mod_info = build_image_layout(bytes)?;
    let deps = load_dependencies(&mod_info, is_user, in_progress)?;
    apply_relocations(&mod_info, &deps)?;

    let module_name = configure_module(&mod_info);

    let descriptor = ModuleDescriptor {
        name: module_name,
        file_handle: Some(file),
        info: mod_info,
        _deps: Some(deps)
    };

    let arc = Arc::new_in(Spinlock::new(descriptor), PoolAllocatorGlobal);
    let weak = Arc::downgrade(&arc);

    KERNEL_MODULES.lock().add_node(weak)
    .expect("Failed to add image reference to module registry");

    info!("Loaded image '{}' with name={}", path, module_name);

    Ok(arc)
}

fn find_loaded_module(path: &str) -> Option<LoadedImage> {
    let resolved = resolve_symlink(path);
    info!("Resolved symlink:{} -> {}", path, resolved);

    let registry = KERNEL_MODULES.lock();
    for node in registry.iter() {
        let entry = match node.upgrade() { Some(e) => e, None => continue };
        let matches = {
            let guard = entry.lock();
            let fh = guard.file_handle.as_ref().unwrap();
            let file_guard = fh.lock();
            file_guard.get_name() == resolved
        };
        if matches {
            return Some(entry);
        }
    }
    None
}

unsafe fn read_cstr<'a>(base: usize, idx: usize) -> &'a str {
    let ptr = (base + idx) as *const i8;
    unsafe {
        CStr::from_ptr(ptr).to_str().unwrap()
    }
}

fn configure_module(info: &ModuleInfo) -> &'static str {
    const FALLBACK: &str = "[none]";
    let dyn_tab = match info.dyn_tab { Some(t) => t, None => return FALLBACK };
    let dyn_str = match info.dyn_str { Some(s) => s, None => return FALLBACK };

    let entries = unsafe {
        core::slice::from_raw_parts(
            dyn_tab.start as *const Elf64Sym,
            dyn_tab.size / dyn_tab.entry_size,
        )
    };

    for sym in entries {
        if sym.st_shndx == SHN_UNDEF {
            continue;
        }
        let sym_name = unsafe { read_cstr(dyn_str.base_address, sym.st_name as usize) };
        if sym_name != "module_config" {
            continue;
        }

        let func_addr = info.base + sym.st_value as usize;
        let func: extern "C" fn() -> StrRef = unsafe { core::mem::transmute(func_addr) };
        let r = func();
        if r.ptr.is_null() || r.len == 0 {
            return FALLBACK;
        }
        return unsafe { r.as_str() };
    }

    FALLBACK
}

fn resolve_import(name: &str, deps: &[LoadedImage]) -> Option<usize> {
    for arc in deps {
        let desc = arc.lock();
        let dyn_tab = match desc.info.dyn_tab { Some(t) => t, None => continue };
        let dyn_str = match desc.info.dyn_str { Some(s) => s, None => continue };

        let entries = unsafe {
            core::slice::from_raw_parts(dyn_tab.start as *const Elf64Sym, dyn_tab.size / dyn_tab.entry_size)
        };
        for sym in entries {
            if sym.st_shndx == SHN_UNDEF {
                continue;
            }
            let sym_name = unsafe { read_cstr(dyn_str.base_address, sym.st_name as usize) };
            if sym_name == name {
                return Some(desc.info.base + sym.st_value as usize);
            }
        }
    }
    None
}

#[derive(Clone)]
struct AuxRegion {
    src_addr: usize,
    dest_addr: usize,
    src_size: usize,
    dest_size: usize,
    entry_size: usize,
}

// Bulk of this code is borrowed from bootloader. See boot/blr/src/arch.rs
fn build_image_layout(bytes: &[u8]) -> Result<ModuleInfo, KError> {
    if bytes.len() < size_of::<Elf64Ehdr>() {
        return Err(KError::InvalidArgument);
    }
    let ehdr = unsafe { &*(bytes.as_ptr() as *const Elf64Ehdr) };
    if ehdr.e_ident[0..4] != [0x7F, b'E', b'L', b'F']
        || ehdr.e_ident[4] != ELFCLASS64
        || ehdr.e_phentsize as usize != size_of::<Elf64Phdr>()
        || ehdr.e_shentsize as usize != size_of::<Elf64Shdr>()
    {
        return Err(KError::InvalidArgument);
    }

    let base = bytes.as_ptr();
    let phdrs = unsafe {
        core::slice::from_raw_parts(base.add(ehdr.e_phoff as usize) as *const Elf64Phdr, ehdr.e_phnum as usize)
    };
    let shdrs = unsafe {
        core::slice::from_raw_parts(base.add(ehdr.e_shoff as usize) as *const Elf64Shdr, ehdr.e_shnum as usize)
    };

    let mut load_regions: Vec<AuxRegion> = Vec::new();
    let mut dyn_shn_region: Option<AuxRegion> = None;
    let mut max_alignment: usize = 0;

    for phdr in phdrs.iter().filter(|p| p.p_type == PT_LOAD || p.p_type == PT_DYNAMIC) {
        let r = AuxRegion {
            src_addr: unsafe { base.add(phdr.p_offset as usize) as usize },
            dest_addr: phdr.p_vaddr as usize,
            src_size: phdr.p_filesz as usize,
            dest_size: phdr.p_memsz as usize,
            entry_size: 0,
        };
        if phdr.p_align != 0 && phdr.p_align != 1 {
            max_alignment = max_alignment.max(phdr.p_align as usize);
        }
        if phdr.p_type == PT_DYNAMIC {
            dyn_shn_region = Some(r.clone());
        }
        load_regions.push(r);
    }
    if load_regions.is_empty() {
        return Err(KError::InvalidArgument);
    }
    load_regions.sort_by(|a, b| a.dest_addr.cmp(&b.dest_addr));

    let mut symtab: Option<AuxRegion> = None;
    let mut symstr: Option<AuxRegion> = None;
    let mut dynsymtab: Option<AuxRegion> = None;
    let mut dynstr: Option<AuxRegion> = None;
    let mut reloc_sections: Vec<AuxRegion> = Vec::new();
    let mut aux_alignment: usize = 0;

    for shdr in shdrs.iter() {
        let ty = shdr.sh_type;
        if ty != SHT_SYMTAB && ty != SHT_RELA && ty != SHT_DYNSYM {
            continue;
        }
        let reg = AuxRegion {
            src_addr: unsafe { base.add(shdr.sh_offset as usize) as usize },
            dest_addr: 0,
            src_size: shdr.sh_size as usize,
            dest_size: 0,
            entry_size: shdr.sh_entsize as usize,
        };
        let link = shdr.sh_link as usize;
        match ty {
            SHT_SYMTAB => {
                let str_shdr = &shdrs[link];
                symtab = Some(reg);
                symstr = Some(AuxRegion {
                    src_addr: unsafe { base.add(str_shdr.sh_offset as usize) as usize },
                    dest_addr: 0,
                    src_size: str_shdr.sh_size as usize,
                    dest_size: 0,
                    entry_size: 0,
                });
            },
            SHT_RELA => reloc_sections.push(reg),
            SHT_DYNSYM => {
                let str_shdr = &shdrs[link];
                dynsymtab = Some(reg);
                dynstr = Some(AuxRegion {
                    src_addr: unsafe { base.add(str_shdr.sh_offset as usize) as usize },
                    dest_addr: 0,
                    src_size: str_shdr.sh_size as usize,
                    dest_size: 0,
                    entry_size: 0,
                });
            },
            _ => {}
        }
        if shdr.sh_addralign != 0 && shdr.sh_addralign != 1 {
            aux_alignment = aux_alignment.max(shdr.sh_addralign as usize);
        }
    }

    let num_reloc_shns = reloc_sections.len();

    if let Some(s) = &symtab {
        reloc_sections.push(s.clone());
        reloc_sections.push(symstr.as_ref().unwrap().clone());
    }
    if let Some(s) = &dynsymtab {
        reloc_sections.push(s.clone());
        reloc_sections.push(dynstr.as_ref().unwrap().clone());
    }

    let last_main = load_regions.last().unwrap();
    let main_shn_size = last_main.dest_addr + last_main.dest_size;

    let mut aux_size = 0usize;
    for shn in reloc_sections.iter() {
        aux_size += shn.src_size;
        if aux_alignment != 0 {
            aux_size += (aux_size as *const u8).align_offset(aux_alignment);
        }
    }

    let aux_padding = if aux_alignment != 0 {
        (main_shn_size as *const u8).align_offset(aux_alignment)
    } else { 0 };
    let aux_shn_end = main_shn_size + aux_padding + aux_size;
    let desc_alignment = align_of::<MemoryRegion>();
    let desc_padding = (aux_shn_end as *const u8).align_offset(desc_alignment);
    let total_module_size = aux_shn_end + desc_padding + num_reloc_shns * size_of::<MemoryRegion>();

    let alloc_align = max_alignment.max(aux_alignment).max(desc_alignment).max(1);
    assert!(alloc_align <= PAGE_SIZE);

    let layout = Layout::from_size_align(total_module_size, PAGE_SIZE).unwrap();
    let load_base = allocate_memory(layout, PageDescriptor::VIRTUAL)?;
    let load_base_addr = load_base.addr();

    for region in load_regions.iter() {
        unsafe {
            let dest = load_base.add(region.dest_addr);
            dest.write_bytes(0, region.dest_size);
            copy_nonoverlapping(region.src_addr as *const u8, dest, region.src_size);
        }
    }

    let mut cur = unsafe { load_base.add(main_shn_size + aux_padding) };
    for region in reloc_sections.iter_mut() {
        unsafe {
            copy_nonoverlapping(region.src_addr as *const u8, cur, region.src_size);
            region.dest_addr = cur.addr();
            cur = cur.add(region.src_size);
            if aux_alignment != 0 {
                cur = cur.add(cur.align_offset(aux_alignment));
            }
        }
    }

    // Strip sym/dynsym pairs in reverse insertion order
    let dynstr_loaded = if dynsymtab.is_some() { reloc_sections.pop() } else { None };
    let dynsymtab_loaded = if dynsymtab.is_some() { reloc_sections.pop() } else { None };
    let symstr_loaded = if symtab.is_some() { reloc_sections.pop() } else { None };
    let symtab_loaded = if symtab.is_some() { reloc_sections.pop() } else { None };

    let rlc_shn_out = if num_reloc_shns > 0 {
        let desc_base = unsafe { load_base.add(aux_shn_end + desc_padding) };
        let descs = unsafe {
            core::slice::from_raw_parts_mut(desc_base as *mut MemoryRegion, num_reloc_shns)
        };
        for (idx, shn) in reloc_sections.iter().enumerate() {
            descs[idx] = MemoryRegion {
                base_address: shn.dest_addr,
                size: shn.src_size,
            };
        }
        Some(ArrayTable {
            start: desc_base.addr(),
            size: num_reloc_shns * size_of::<MemoryRegion>(),
            entry_size: size_of::<MemoryRegion>(),
        })
    } else { None };

    let sym_tab_out = symtab_loaded.as_ref().map(|s| ArrayTable {
        start: s.dest_addr, size: s.src_size, entry_size: s.entry_size,
    });
    let sym_str_out = symstr_loaded.as_ref().map(|s| MemoryRegion {
        base_address: s.dest_addr, size: s.src_size,
    });
    let dyn_tab_out = dynsymtab_loaded.as_ref().map(|s| ArrayTable {
        start: s.dest_addr, size: s.src_size, entry_size: s.entry_size,
    });
    let dyn_str_out = dynstr_loaded.as_ref().map(|s| MemoryRegion {
        base_address: s.dest_addr, size: s.src_size,
    });
    let dyn_shn_out = dyn_shn_region.as_ref().map(|s| ArrayTable {
        start: load_base_addr + s.dest_addr,
        size: s.src_size,
        entry_size: size_of::<ElfDyn>(),
    });

    Ok(ModuleInfo {
        entry: ehdr.e_entry as usize + load_base_addr,
        base: load_base_addr,
        size: main_shn_size,
        total_size: total_module_size,
        sym_tab: sym_tab_out,
        sym_str: sym_str_out,
        dyn_tab: dyn_tab_out,
        dyn_str: dyn_str_out,
        rlc_shn: rlc_shn_out,
        dyn_shn: dyn_shn_out,
    })
}

fn load_dependencies(
    mod_info: &ModuleInfo,
    is_user: bool,
    in_progress: &mut Vec<String>
) -> Result<Vec<LoadedImage>, KError> {
    let mut deps: Vec<LoadedImage> = Vec::new();

    let dyn_shn = match mod_info.dyn_shn { Some(d) => d, None => return Ok(deps) };
    let dyn_str = match mod_info.dyn_str { Some(d) => d, None => return Ok(deps) };

    let num_entries = dyn_shn.size / dyn_shn.entry_size;
    let entries = unsafe {
        core::slice::from_raw_parts(dyn_shn.start as *const ElfDyn, num_entries)
    };

    const PREDEFINED_DIRECTORIES: [&str; 3] = ["", "/sys", "/sys/drivers"];

    for entry in entries {
        if entry.tag == DT_NULL {
            break;
        }
        if entry.tag != DT_NEEDED {
            continue;
        }
        let name = unsafe { read_cstr(dyn_str.base_address, entry.val as usize) };
        let mut found_entry = false;

        // This is an absolute path, use it directly
        if name.starts_with("/") {
            let res = load_image_inner(
                name,
                is_user,
                in_progress
            )?;
            
            info!("Loaded dependency {}", name);
            deps.push(res);
        }
        else {
            // Check for this image in all of these predefined directories
            for prefix in PREDEFINED_DIRECTORIES {
                let filename = format!("{}/{}", prefix, name);
                let res = load_image_inner(
                    filename.as_str(),
                    is_user,
                    in_progress
                );

                match res {
                    Err(InvalidArgument) => {
                        continue;
                    },
                    Err(e) => {
                        return Err(e);
                    },
                    Ok(dep) => {
                        info!("Loaded dependency {}", filename);
                        found_entry = true;
                        deps.push(dep);
                        break;
                    }
                }
            }
            
            if !found_entry {
                return Err(KError::InvalidArgument);
            }
        } 
    }

    Ok(deps)
}

fn apply_relocations(
    mod_info: &ModuleInfo,
    deps: &[LoadedImage]
) -> Result<(), KError> {
    let rlc_shn = match mod_info.rlc_shn { Some(r) => r, None => return Ok(()) };

    let load_base = mod_info.base;
    let dyn_tab = mod_info.dyn_tab;
    let dyn_str = mod_info.dyn_str;

    let info_field = |bitmap: u64| (bitmap & 0xffffffff) as u32;

    let reloc_sections = unsafe {
        core::slice::from_raw_parts(rlc_shn.start as *const MemoryRegion, rlc_shn.size / rlc_shn.entry_size)
    };

    for shn in reloc_sections {
        let num_entries = shn.size / size_of::<Elf64Rela>();
        let entries = unsafe {
            core::slice::from_raw_parts(shn.base_address as *const Elf64Rela, num_entries)
        };

        for entry in entries {
            let address = load_base + entry.r_offset as usize;
            let rtype = info_field(entry.r_info);
            match rtype {
                R_X86_64_RELATIVE => unsafe {
                    *(address as *mut u64) = (load_base as i64 + entry.r_addend) as u64;
                },
                R_X86_64_64 | R_GLOB_DAT | R_JUMP_SLOT => {
                    let dyn_tab = dyn_tab.expect("Failed to find dyn_tab entry! Corrupted image?");
                    let dyn_entries = unsafe {
                        core::slice::from_raw_parts(dyn_tab.start as *const Elf64Sym, dyn_tab.size / dyn_tab.entry_size)
                    };
                    let sym_idx = (entry.r_info >> 32) as usize;
                    let sym = &dyn_entries[sym_idx];

                    let resolved = if sym.st_shndx == SHN_UNDEF {
                        let dyn_str = dyn_str.expect("Failed to find dyn_str entry! Corrupted image?");
                        let name = unsafe { read_cstr(dyn_str.base_address, sym.st_name as usize) };
                        match resolve_import(name, deps) {
                            Some(v) => v,
                            None => {
                                info!("Unresolved import symbol: {}", name);
                                return Err(KError::InvalidArgument);
                            }
                        }
                    } else {
                        load_base + sym.st_value as usize
                    };

                    let value = if rtype == R_JUMP_SLOT {
                        resolved
                    } else {
                        (resolved as i64 + entry.r_addend) as usize
                    };

                    unsafe { *(address as *mut u64) = value as u64; }
                },
                _ => {}
            }
        }
    }

    Ok(())
}