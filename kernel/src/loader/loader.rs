use alloc::sync::{Arc, Weak};
use alloc::format;
use alloc::vec::Vec;
use alloc::string::String;
use alloc::borrow::ToOwned;
use kernel_intf::KError::InvalidArgument;
use core::alloc::Layout;
use core::mem::size_of;
use core::ptr::copy_nonoverlapping;
use core::ffi::CStr;
use common::{PAGE_SIZE, elf::*};
use common::{ArrayTable, MemoryRegion, ModuleInfo, StrRef};
use kernel_intf::{KError, info};
use crate::KERNEL_PATH;
use crate::fs::{self, FileBuffer, open};
use crate::infra::disable_preloader_phase;
use crate::loader::module::{KernelModule, ModuleDescriptor, ModuleType};
use crate::loader::user_loader::load_user_image;
use crate::mem::{PageDescriptor, allocate_memory, deallocate_memory};
use crate::sched::get_current_process;
use kernel_intf::mem::PoolAllocatorGlobal;
use crate::sync::{KSem, Once, Spinlock, semaphore_guard};
use kernel_intf::list::{List, DynList};
use super::module;

pub static KERNEL_MODULES: Spinlock<DynList<LoadedImageWeak>> = Spinlock::new(List::new());
pub(super) const PREDEFINED_DIRECTORIES: [&str; 3] = ["", "/sys", "/sys/drivers"];

static LOAD_LOCK: Once<KSem> = Once::new();

pub type LoadedImage = Arc<Spinlock<ModuleDescriptor>, PoolAllocatorGlobal>;
pub type LoadedImageWeak = Weak<Spinlock<ModuleDescriptor>, PoolAllocatorGlobal>; 

impl Drop for ModuleDescriptor {
    fn drop(&mut self) {
        match &self.mod_type {
            ModuleType::Kernel(kmod) => {
                {
                    let mut registry = KERNEL_MODULES.lock();
                    crate::loader_log!("Dropping image {}, with registry_len={}", kmod.name, registry.get_nodes());

                    // Cleanup: Remove all weak refs from the registry
                    while registry.find_and_remove(|entry| {
                        let item = entry.upgrade();
                        item.is_none()
                    }).is_some() {}

                    crate::loader_log!("New registry len = {}", registry.get_nodes());
                }

                deallocate_memory(
                    kmod.info.base as *mut u8,
                    Layout::from_size_align(kmod.info.total_size, PAGE_SIZE).unwrap(),
                    PageDescriptor::VIRTUAL
                ).expect("Failed to deallocate backing memory for kernel module!");
            },
            ModuleType::User(_) => {
                // The outer descriptor only carries this process's base/entry
                // and a strong ref to the shared state. Dropping it simply
                // decrements the shared Arc; the shared cleanup (registry purge
                // + read-only phys release) lives in SharedUserModule::drop
            }
        }
    }
}

impl ModuleDescriptor {
    // Loads and gives the address of an exported symbol
    pub fn load_symbol(&self, symbol: &str) -> Option<usize> {
        let info = &self.kernel().info;
        let dyn_tab = match info.dyn_tab { Some(t) => t, None => return None };
        let dyn_str = match info.dyn_str { Some(s) => s, None => return None };

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
            if sym_name == symbol {
                let func_addr = info.base + sym.st_value as usize;
                return Some(func_addr);
            }
        }

        None
    }
}

pub fn invoke_init(image: &LoadedImage) {
    let entry = image.lock().kernel().info.entry;

    let init_fn: extern "C" fn() = unsafe { core::mem::transmute(entry) };
    init_fn();
}


pub fn init() {
    LOAD_LOCK.call_once(|| KSem::new(1, 1));
    super::user_loader::init();

    let mut kernel_img = module::ARIS.get().unwrap().lock().clone();
    kernel_img.kernel_mut().canonical_file_path = Some(KERNEL_PATH.to_owned());

    let loaded_img = Arc::new_in(
        Spinlock::new(
            kernel_img
        ),
        PoolAllocatorGlobal
    );

    let downgraded_ref = Arc::downgrade(&loaded_img);
    get_current_process().unwrap().lock().set_image(loaded_img);
    KERNEL_MODULES.lock().add_node(downgraded_ref)
    .expect("Failed to add kernel image module to Loaded images registry!");

    disable_preloader_phase();
}

pub fn load_image(path: &str, is_user: bool, run_init: bool) -> Result<LoadedImage, KError> {
    crate::loader_log!("Start load_image for {}", path);

    // User modules go through the user loader, which has its own lock and
    // per-process bookkeeping
    if is_user {
        return load_user_image(path);
    }

    // Serialize the whole recursive load
    let _guard = semaphore_guard(LOAD_LOCK.get().expect("loader::init() not called before load_image()"));

    let mut in_progress: Vec<String> = Vec::new();
    load_image_inner(path, &mut in_progress, &[], run_init)
}


fn do_load_image_inner(
    path: &str,
    in_progress: &mut Vec<String>,
    run_init: bool
) -> Result<LoadedImage, KError> {
    if let Some(cached) = find_loaded_module(path) {
        crate::loader_log!("Loading image {} from cache", path);
        return Ok(cached);
    }

    if in_progress.iter().any(|n| n == path) {
        return Err(KError::CircularDependency);
    }
    in_progress.push(path.to_owned());

    let result = load_image_uncached(
        path,
        in_progress,
        run_init
    );

    in_progress.pop();
    result
}

fn load_image_inner(
    path: &str,
    in_progress: &mut Vec<String>,
    extra_dirs: &[String],
    run_init: bool
) -> Result<LoadedImage, KError> {
    // This is an absolute path
    if path.starts_with("/") {
        return do_load_image_inner(path, in_progress, run_init);
    }
    else {
        // Check for the file in all the predefined directories, then any
        // caller-supplied extra directories
        for prefix in PREDEFINED_DIRECTORIES.iter().copied().chain(extra_dirs.iter().map(|s| s.as_str())) {
            let filename = format!("{}/{}", prefix, path);
            let res = do_load_image_inner(filename.as_str(), in_progress, run_init);

            match res {
                Err(KError::NotFound) | Err(InvalidArgument) => {
                    continue;
                },
                Err(e) => {
                    return Err(e);
                },
                Ok(img) => {
                    return Ok(img)
                }
            }
        }

        // Check with cwd too
        let filename = fs::make_absolute(&crate::sched::get_cwd(), path);
        let res = do_load_image_inner(filename.as_str(), in_progress, run_init);

        match res {
            Ok(img) => {
                return Ok(img)
            },
            _ => {}
        }
    }

    Err(KError::NotFound)
}

fn load_image_uncached(
    path: &str,
    in_progress: &mut Vec<String>,
    run_init: bool
) -> Result<LoadedImage, KError> {
    crate::loader_log!("Loading image {} from disk", path);
    let file = open(path)?;
    let file_size = file.len();
    let canonical_file_path = Some(fs::resolve_symlink(&path).expect("File path could not be resolved!"));

    let buf= FileBuffer::new(file_size, false)
    .or_else(|e| {
        info!("Failed to allocate filebuffer for image {}", path);
        Err(e)
    })?;

    let read_len = file.read(&buf).map_err(|e| {
        info!("Read failed for image {}: {}", path, e);
        e
    })?;
    if read_len != file_size {
        info!("read_len={} doesn't seem to match file_size={} for path={}", read_len, file_size, path);
        return Err(KError::InvalidArgument);
    }
    let bytes = buf.as_slice();

    let mod_info = build_image_layout(bytes)?;
    let deps = load_dependencies(&mod_info, in_progress)?;
    apply_relocations(&mod_info, &deps)?;

    let (module_name, driver_init_address, driver_unload_address) = configure_module(&mod_info);

    let module_name = module_name.ok_or_else(|| {
        info!("Image at path {} is not valid kernel module!", path);
        KError::InvalidArgument
    })?;

    let descriptor = ModuleDescriptor {
        mod_type: ModuleType::Kernel(KernelModule {
            name: module_name,
            driver_init_address,
            driver_unload_address,
            canonical_file_path,
            info: mod_info,
            _deps: Some(deps)
        })
    };

    let arc = Arc::new_in(Spinlock::new(descriptor), PoolAllocatorGlobal);
    let weak = Arc::downgrade(&arc);

    KERNEL_MODULES.lock().add_node(weak)
    .expect("Failed to add image reference to module registry");

    crate::loader_log!("Loaded image '{}' with name={}", path, module_name);

    if run_init {
        invoke_init(&arc);
    }

    Ok(arc)
}

fn find_loaded_module(path: &str) -> Option<LoadedImage> {
    let canonical = fs::resolve_symlink(&path).unwrap_or_else(|_| path.to_owned());
    crate::loader_log!("find_loaded_module: looking for {} (canonical: {})", path, canonical);

    // Collect strong refs under the registry lock, but compare (and drop the
    // non-matching Arcs) outside of it. Dropping the last strong ref runs
    // ModuleDescriptor::drop which re-locks the registry — doing that while
    // holding the lock would self-deadlock.
    let candidates: Vec<LoadedImage> = {
        let registry = KERNEL_MODULES.lock();
        registry.iter().filter_map(|node| node.upgrade()).collect()
    };

    for entry in candidates {
        let matches = {
            let guard = entry.lock();
            let fh = guard.kernel().canonical_file_path.as_ref().unwrap();
            *fh == canonical
        };
        if matches {
            return Some(entry);
        }
    }
    None
}

pub(super) unsafe fn read_cstr<'a>(base: usize, idx: usize) -> &'a str {
    let ptr = (base + idx) as *const i8;
    unsafe {
        CStr::from_ptr(ptr).to_str().unwrap()
    }
}

fn configure_module(info: &ModuleInfo) -> (Option<&'static str>, Option<usize>, Option<usize>) {
    let mut res_str: Option<&str> = None;
    let mut driver_init_addr = None;
    let mut driver_unload_addr = None;
    let dyn_tab = match info.dyn_tab { Some(t) => t, None => return (res_str, None, None) };
    let dyn_str = match info.dyn_str { Some(s) => s, None => return (res_str, None, None) };

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
        if sym_name == "_shim_module_config" {
            let func_addr = info.base + sym.st_value as usize;
            let func: extern "C" fn() -> StrRef = unsafe { core::mem::transmute(func_addr) };
            let r = func();
            if r.ptr.is_null() || r.len == 0 {
                break;
            }
            else {
                res_str = Some(unsafe { r.as_str() });
            }
        }
        else if sym_name == "_shim_driver_init" {
            let func_addr = info.base + sym.st_value as usize;
            driver_init_addr = Some(func_addr);
        }
        else if sym_name == "_shim_driver_unload" {
            let func_addr = info.base + sym.st_value as usize;
            driver_unload_addr = Some(func_addr);
        }
    }

    (res_str, driver_init_addr, driver_unload_addr)
}

fn resolve_import(name: &str, deps: &[LoadedImage]) -> Option<usize> {
    for arc in deps {
        let desc = arc.lock();
        let dyn_tab = match desc.kernel().info.dyn_tab { Some(t) => t, None => continue };
        let dyn_str = match desc.kernel().info.dyn_str { Some(s) => s, None => continue };

        let entries = unsafe {
            core::slice::from_raw_parts(dyn_tab.start as *const Elf64Sym, dyn_tab.size / dyn_tab.entry_size)
        };
        for sym in entries {
            if sym.st_shndx == SHN_UNDEF {
                continue;
            }
            let sym_name = unsafe { read_cstr(dyn_str.base_address, sym.st_name as usize) };
            if sym_name == name {
                return Some(desc.kernel().info.base + sym.st_value as usize);
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
        assert!(phdr.p_vaddr as usize % PAGE_SIZE == 0, "Loadable segment VMA {:#X} is not page-aligned", phdr.p_vaddr);
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
    let mut aux_alignment: usize = 0;

    for shdr in shdrs.iter() {
        if shdr.sh_type != SHT_SYMTAB {
            continue;
        }
        let reg = AuxRegion {
            src_addr: unsafe { base.add(shdr.sh_offset as usize) as usize },
            dest_addr: 0,
            src_size: shdr.sh_size as usize,
            dest_size: 0,
            entry_size: shdr.sh_entsize as usize
        };
        let str_shdr = &shdrs[shdr.sh_link as usize];
        symtab = Some(reg);
        symstr = Some(AuxRegion {
            src_addr: unsafe { base.add(str_shdr.sh_offset as usize) as usize },
            dest_addr: 0,
            src_size: str_shdr.sh_size as usize,
            dest_size: 0,
            entry_size: 0,
        });
        if shdr.sh_addralign != 0 && shdr.sh_addralign != 1 {
            aux_alignment = aux_alignment.max(shdr.sh_addralign as usize);
        }
        break;
    }

    let last_main = load_regions.last().unwrap();
    let main_shn_size = last_main.dest_addr + last_main.dest_size;

    // Aux area holds symtab + symstr (debug-symbol pair only).
    let aux_size = match (&symtab, &symstr) {
        (Some(st), Some(ss)) => {
            let pad = if aux_alignment != 0 {
                (st.src_size as *const u8).align_offset(aux_alignment)
            } else { 0 };
            st.src_size + pad + ss.src_size
        },
        _ => 0,
    };

    let aux_padding = if aux_alignment != 0 {
        (main_shn_size as *const u8).align_offset(aux_alignment)
    } else { 0 };
    let aux_shn_end = main_shn_size + aux_padding + aux_size;
    let total_module_size = aux_shn_end;

    let alloc_align = max_alignment.max(aux_alignment).max(1);
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

    // Copy symtab + symstr to aux area.
    let (sym_tab_out, sym_str_out) = if let (Some(mut st), Some(mut ss)) = (symtab, symstr) {
        let mut cur = unsafe { load_base.add(main_shn_size + aux_padding) };
        unsafe {
            copy_nonoverlapping(st.src_addr as *const u8, cur, st.src_size);
            st.dest_addr = cur.addr();
            cur = cur.add(st.src_size);
            if aux_alignment != 0 {
                cur = cur.add(cur.align_offset(aux_alignment));
            }
            copy_nonoverlapping(ss.src_addr as *const u8, cur, ss.src_size);
            ss.dest_addr = cur.addr();
        }
        (
            Some(ArrayTable { start: st.dest_addr, size: st.src_size, entry_size: st.entry_size }),
            Some(MemoryRegion { base_address: ss.dest_addr, size: ss.src_size }),
        )
    } else {
        (None, None)
    };

    let dyn_shn_out = dyn_shn_region.as_ref().map(|s| ArrayTable {
        start: load_base_addr + s.dest_addr,
        size:  s.src_size,
        entry_size: size_of::<ElfDyn>(),
    });

    // Resolve dynsymtab / dynstr / .rela.dyn locations from the loaded
    // PT_DYNAMIC entries.
    let dyn_info = dyn_shn_out.as_ref().map(|d| parse_dynamic_section(load_base_addr, d));
    let dyn_tab_out = dyn_info.as_ref().and_then(|i| i.dyn_tab);
    let dyn_str_out = dyn_info.as_ref().and_then(|i| i.dyn_str);
    let rlc_shn_out = dyn_info.as_ref().and_then(|i| i.rela);
    let plt_shn_out = dyn_info.as_ref().and_then(|i| i.plt);
    let rlc_count_out = dyn_info.as_ref().and_then(|i| Some(i.relacount)).unwrap_or(0);

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
        plt_shn: plt_shn_out,
        dyn_shn: dyn_shn_out,
        rlc_count: rlc_count_out
    })
}

struct DynamicInfo {
    dyn_tab: Option<ArrayTable>,
    dyn_str: Option<MemoryRegion>,
    rela:    Option<ArrayTable>,
    relacount: usize,
    plt: Option<ArrayTable>
}

fn parse_dynamic_section(load_base: usize, dyn_shn: &ArrayTable) -> DynamicInfo {
    let nentries = dyn_shn.size / size_of::<ElfDyn>();
    let entries = unsafe {
        core::slice::from_raw_parts(dyn_shn.start as *const ElfDyn, nentries)
    };

    let mut symtab_vma: Option<usize> = None;
    let mut strtab_vma: Option<usize> = None;
    let mut rela_vma:   Option<usize> = None;
    let mut plt_vma: Option<usize> = None;
    let mut hash_vma:   Option<usize> = None;
    let mut strsz  = 0usize;
    let mut relasz = 0usize;
    let mut relaent = size_of::<Elf64Rela>();
    let mut relacount = 0usize;
    let mut pltsize = 0usize;

    for dynent in entries {
        match dynent.tag {
            DT_NULL    => break,
            DT_SYMTAB  => symtab_vma = Some(dynent.val as usize),
            DT_STRTAB  => strtab_vma = Some(dynent.val as usize),
            DT_STRSZ   => strsz      = dynent.val as usize,
            DT_RELA    => rela_vma   = Some(dynent.val as usize),
            DT_RELASZ  => relasz     = dynent.val as usize,
            DT_JMPREL => plt_vma = Some(dynent.val as usize),
            DT_PLTRELSZ => pltsize = dynent.val as usize,
            DT_RELAENT => {
                relaent = dynent.val as usize;
                assert_eq!(relaent, size_of::<Elf64Rela>());
            },
            DT_RELACOUNT => relacount = dynent.val as usize,
            DT_HASH    => hash_vma   = Some(dynent.val as usize),
            _ => {}
        }
    }

    // Derive symtab size from DT_HASH: nchain (the 2nd 32-bit word) holds nsyms.
    let symtab_size = hash_vma.map(|h| unsafe {
        let nchain = *((load_base + h) as *const u32).add(1);
        nchain as usize * size_of::<Elf64Sym>()
    }).unwrap_or(0);

    DynamicInfo {
        dyn_tab: symtab_vma.map(|v| ArrayTable {
            start: load_base + v,
            size:  symtab_size,
            entry_size: size_of::<Elf64Sym>(),
        }),
        dyn_str: strtab_vma.map(|v| MemoryRegion {
            base_address: load_base + v,
            size: strsz,
        }),
        rela: rela_vma.map(|v| ArrayTable {
            start: load_base + v,
            size: relasz,
            entry_size: relaent,
        }),
        plt: plt_vma.map(|v| ArrayTable {
            start: load_base + v,
            size: pltsize,
            entry_size: relaent
        }),
        relacount
    }
}

// Reads this module's own DT_RUNPATH (or DT_RPATH, as a fallback) entry and
// splits it into individual directories, standard ELF colon-separated style
fn parse_runpath(entries: &[ElfDyn], dyn_str_base: usize) -> Vec<String> {
    let mut runpath_off = None;
    let mut rpath_off = None;

    for entry in entries {
        match entry.tag {
            DT_NULL => break,
            DT_RUNPATH => runpath_off = Some(entry.val as usize),
            DT_RPATH => rpath_off = Some(entry.val as usize),
            _ => {}
        }
    }

    match runpath_off.or(rpath_off) {
        Some(off) => {
            let raw = unsafe { read_cstr(dyn_str_base, off) };
            raw.split(':').filter(|s| !s.is_empty()).map(|s| s.to_owned()).collect()
        },
        None => Vec::new()
    }
}

fn load_dependencies(
    mod_info: &ModuleInfo,
    in_progress: &mut Vec<String>
) -> Result<Vec<LoadedImage>, KError> {
    let mut deps: Vec<LoadedImage> = Vec::new();

    let dyn_shn = match mod_info.dyn_shn { Some(d) => d, None => return Ok(deps) };
    let dyn_str = match mod_info.dyn_str { Some(d) => d, None => return Ok(deps) };

    let num_entries = dyn_shn.size / dyn_shn.entry_size;
    let entries = unsafe {
        core::slice::from_raw_parts(dyn_shn.start as *const ElfDyn, num_entries)
    };

    let extra_dirs = parse_runpath(entries, dyn_str.base_address);

    for entry in entries {
        if entry.tag == DT_NULL {
            break;
        }
        if entry.tag != DT_NEEDED {
            continue;
        }
        let name = unsafe { read_cstr(dyn_str.base_address, entry.val as usize) };

        crate::loader_log!("Loading dependency {}", name);
        let res = load_image_inner(
            name,
            in_progress,
            &extra_dirs,
            true
        )?;

        deps.push(res);
    }

    Ok(deps)
}

fn process_reloc_shn(
    load_base:usize, 
    dyn_tab: Option<ArrayTable>, 
    dyn_str: Option<MemoryRegion>, 
    entries: &[Elf64Rela],
    deps: &[LoadedImage]
) -> Result<(), KError> {
    let info_field = |bitmap: u64| (bitmap & 0xffffffff) as u32;
    for entry in entries {
        let address = load_base + entry.r_offset as usize;
        let rtype = info_field(entry.r_info);
        match rtype {
            R_X86_64_RELATIVE => unsafe {
                *(address as *mut u64) = (load_base as i64 + entry.r_addend) as u64;
            },
            R_X86_64_64 | R_GLOB_DAT | R_JUMP_SLOT => {
                let dyn_tab = dyn_tab.expect("Failed to find dyn_tab entry! Corrupted image?");
                let sym_idx = (entry.r_info >> 32) as usize;
                let sym = unsafe {
                    &*(dyn_tab.start as *const Elf64Sym).add(sym_idx)
                };

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

    Ok(())
}

fn apply_relocations(
    mod_info: &ModuleInfo,
    deps: &[LoadedImage]
) -> Result<(), KError> {
    if mod_info.rlc_shn.is_none() && mod_info.plt_shn.is_none() {
        return Ok(())
    }

    let load_base = mod_info.base;
    let dyn_tab = mod_info.dyn_tab;
    let dyn_str = mod_info.dyn_str;


    let num_entries_rlc = mod_info.rlc_shn.map(|r| r.size / r.entry_size).unwrap_or(0);
    let entries_rlc = mod_info.rlc_shn.map(|r| {
        unsafe {
            core::slice::from_raw_parts(r.start as *const Elf64Rela, r.size / r.entry_size)
        }
    });

    let num_entries_plt = mod_info.plt_shn.map(|r| {r.size / r.entry_size}).unwrap_or(0);
    let entries_plt = mod_info.plt_shn.map(|r| {
        unsafe {
            core::slice::from_raw_parts(r.start as *const Elf64Rela, r.size / r.entry_size)
        }
    });

    if num_entries_rlc > 0 {
        process_reloc_shn(load_base, dyn_tab, dyn_str, entries_rlc.unwrap(), deps)?;
    }
    
    if num_entries_plt > 0 {
        process_reloc_shn(load_base, dyn_tab, dyn_str, entries_plt.unwrap(), deps)?;
    }

    Ok(())
}